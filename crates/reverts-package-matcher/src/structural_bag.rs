//! Aggregate structural-bag package ownership matcher.
//!
//! This matcher intentionally runs late in the package matching pipeline. It
//! does not try to prove a unique importable file; instead it compares the
//! multiset of per-function structural fingerprint axes in a bundle module
//! against the aggregate multiset for a package version. That makes it useful
//! when real bundle minification changed function names, split package files
//! were concatenated into one module, or CJS/ESM export wrappers prevent exact
//! source matching.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use reverts_graph::FunctionExtractor;
use reverts_input::{InputRows, ModuleInput, PackageAttributionStatus, PackageEmissionMode};
use reverts_ir::{AxisHashes, AxisKind, FunctionFingerprint, ModuleId, ModuleKind};
use reverts_observe::AuditReport;
use semver::Version;

use crate::{
    ModuleMatchStrategy, PackageMatch, PackageModuleSourceQuality, PackageSource,
    package_module_source_quality,
};

/// Result of aggregate structural-bag ownership matching.
#[derive(Debug, Clone, PartialEq)]
pub struct StructuralBagMatchReport {
    /// Source-only module-to-package ownership matches.
    pub matches: Vec<PackageMatch>,
    /// Parse or indexing findings produced while building structural bags.
    pub audit: AuditReport,
}

/// Matches package modules to package versions by aggregate structural axes.
///
/// Returned matches are ownership-only (`external_importable = false`). Callers
/// should append them after exact/hash, signature, and cascade promotion have
/// had a chance to prove stronger evidence.
#[must_use]
pub fn match_structural_bags(
    rows: &InputRows,
    package_sources: &[PackageSource],
    package_filter: Option<&BTreeSet<String>>,
) -> StructuralBagMatchReport {
    let mut audit = AuditReport::default();
    let module_bags = candidate_modules(rows, package_filter)
        .filter_map(|module| build_module_bag(rows, module))
        .collect::<Vec<_>>();
    let needed_packages = module_bags
        .iter()
        .filter_map(|module_bag| module_bag.package_name.clone())
        .collect::<BTreeSet<_>>();
    if needed_packages.is_empty() {
        return StructuralBagMatchReport {
            matches: Vec::new(),
            audit,
        };
    }
    let package_bags = build_package_bags(package_sources, &needed_packages, &mut audit);
    let mut matches = Vec::new();

    for module_bag in module_bags {
        let Some(module_package_name) = module_bag.package_name.as_deref() else {
            continue;
        };
        let candidates = candidate_package_bags(&package_bags, &module_bag);
        if candidates.is_empty() {
            continue;
        }
        if let Some(score) = best_package_score(&module_bag, candidates.as_slice()) {
            matches.push(PackageMatch {
                module_id: module_bag.module_id,
                package_name: module_package_name.to_string(),
                package_version: score.bag.package_version.clone(),
                export_specifier: module_package_name.to_string(),
                source_path: structural_bag_source_path(&score),
                normalized_source_hash: String::new(),
                strategy: ModuleMatchStrategy::AggregateStructuralBagSimilarity,
                function_signature_matches: score.matched_functions,
                string_anchor_matches: score.matched_strong_axes,
                external_importable: false,
            });
        }
    }

    StructuralBagMatchReport { matches, audit }
}

fn candidate_modules<'a>(
    rows: &'a InputRows,
    package_filter: Option<&'a BTreeSet<String>>,
) -> impl Iterator<Item = &'a ModuleInput> + 'a {
    rows.modules.iter().filter(move |module| {
        module.kind == ModuleKind::Package
            && !has_accepted_external_attribution(rows, module.id)
            && package_filter.map_or(true, |filter| {
                module
                    .package_name
                    .as_deref()
                    .is_some_and(|package_name| filter.contains(package_name))
            })
    })
}

fn has_accepted_external_attribution(rows: &InputRows, module_id: ModuleId) -> bool {
    rows.package_attributions.iter().any(|attribution| {
        attribution.module_id == module_id
            && attribution.status == PackageAttributionStatus::Accepted
            && attribution.emission_mode == PackageEmissionMode::ExternalImport
    })
}

#[derive(Debug, Clone)]
struct ModuleStructuralBag {
    module_id: ModuleId,
    package_name: Option<String>,
    package_version: Option<String>,
    axis_counts: BTreeMap<AxisKey, usize>,
    total_weight: u32,
    functions: Vec<FunctionStructuralBag>,
}

#[derive(Debug, Clone)]
struct PackageStructuralBag {
    package_name: String,
    package_version: String,
    axis_counts: BTreeMap<AxisKey, usize>,
    axis_function_counts: BTreeMap<AxisKey, usize>,
    total_weight: u32,
    function_count: usize,
}

#[derive(Debug, Clone)]
struct FunctionStructuralBag {
    keys: BTreeSet<AxisKey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct AxisKey {
    param_count: u32,
    kind: AxisKind,
    hash: u64,
}

#[derive(Debug, Clone)]
struct PackageScore<'a> {
    bag: &'a PackageStructuralBag,
    matched_weight: u32,
    module_weight: u32,
    coverage: f64,
    function_coverage: f64,
    margin: f64,
    score: f64,
    matched_strong_axes: usize,
    matched_functions: usize,
}

fn build_module_bag(rows: &InputRows, module: &ModuleInput) -> Option<ModuleStructuralBag> {
    let slice = rows.module_source_slice(module.id)?;
    match package_module_source_quality(module, slice.source_file_path, slice.source) {
        PackageModuleSourceQuality::Trusted | PackageModuleSourceQuality::Weak => {}
        PackageModuleSourceQuality::Invalid => return None,
    }
    let fingerprints = FunctionExtractor::fingerprint(module.id, slice.source);
    if fingerprints.is_empty() {
        return None;
    }
    let functions = structural_functions(&fingerprints);
    if functions.is_empty() {
        return None;
    }
    let (axis_counts, total_weight) = axis_counts_for_functions(&functions);
    if total_weight == 0 {
        return None;
    }
    Some(ModuleStructuralBag {
        module_id: module.id,
        package_name: module.package_name.clone(),
        package_version: module.package_version.clone(),
        axis_counts,
        total_weight,
        functions,
    })
}

fn build_package_bags(
    package_sources: &[PackageSource],
    needed_packages: &BTreeSet<String>,
    _audit: &mut AuditReport,
) -> Vec<PackageStructuralBag> {
    let mut grouped = BTreeMap::<(String, String), Vec<FunctionStructuralBag>>::new();
    for (idx, source) in package_sources.iter().enumerate() {
        if !needed_packages.contains(source.package_name.as_str()) {
            continue;
        }
        let synthetic_module_id = ModuleId(u32::MAX.saturating_sub(idx as u32));
        let fingerprints =
            FunctionExtractor::fingerprint(synthetic_module_id, source.source.as_str());
        let functions = structural_functions(&fingerprints);
        if functions.is_empty() {
            continue;
        }
        grouped
            .entry((source.package_name.clone(), source.package_version.clone()))
            .or_default()
            .extend(functions);
    }

    grouped
        .into_iter()
        .filter_map(|((package_name, package_version), functions)| {
            let function_count = functions.len();
            if function_count == 0 {
                return None;
            }
            let (axis_counts, total_weight) = axis_counts_for_functions(&functions);
            if total_weight == 0 {
                return None;
            }
            let mut axis_function_counts = BTreeMap::<AxisKey, usize>::new();
            for function in &functions {
                for key in &function.keys {
                    *axis_function_counts.entry(*key).or_default() += 1;
                }
            }
            Some(PackageStructuralBag {
                package_name,
                package_version,
                axis_counts,
                axis_function_counts,
                total_weight,
                function_count,
            })
        })
        .collect()
}

fn structural_functions(fingerprints: &[FunctionFingerprint]) -> Vec<FunctionStructuralBag> {
    fingerprints
        .iter()
        .filter_map(|fingerprint| {
            let mut keys = axis_keys_for_axes(fingerprint.param_count, &fingerprint.primary);
            for alternate in &fingerprint.alternates {
                keys.extend(axis_keys_for_axes(fingerprint.param_count, &alternate.axes));
            }
            if keys.is_empty() {
                None
            } else {
                Some(FunctionStructuralBag { keys })
            }
        })
        .collect()
}

fn axis_counts_for_functions(
    functions: &[FunctionStructuralBag],
) -> (BTreeMap<AxisKey, usize>, u32) {
    let mut axis_counts = BTreeMap::<AxisKey, usize>::new();
    let mut total_weight = 0u32;
    for function in functions {
        for key in &function.keys {
            *axis_counts.entry(*key).or_default() += 1;
            total_weight = total_weight.saturating_add(axis_weight(key.kind));
        }
    }
    (axis_counts, total_weight)
}

fn axis_keys_for_axes(param_count: u32, axes: &AxisHashes) -> BTreeSet<AxisKey> {
    let mut keys = BTreeSet::new();
    push_axis_key(&mut keys, param_count, AxisKind::Ast, axes.ast);
    push_axis_key(&mut keys, param_count, AxisKind::Cfg, axes.cfg);
    push_axis_key(
        &mut keys,
        param_count,
        AxisKind::ReturnPattern,
        axes.return_pattern,
    );
    push_axis_key(
        &mut keys,
        param_count,
        AxisKind::EffectPattern,
        axes.effect_pattern,
    );
    push_axis_key(
        &mut keys,
        param_count,
        AxisKind::StructuralAnchor,
        axes.structural_anchor,
    );
    push_axis_key(
        &mut keys,
        param_count,
        AxisKind::BindingPattern,
        axes.binding_pattern,
    );
    if let Some(hash) = axes.literal_anchor {
        push_axis_key(&mut keys, param_count, AxisKind::LiteralAnchor, hash);
    }
    if let Some(hash) = axes.access_pattern {
        push_axis_key(&mut keys, param_count, AxisKind::AccessPattern, hash);
    }
    if let Some(hash) = axes.literal_shape {
        push_axis_key(&mut keys, param_count, AxisKind::LiteralShape, hash);
    }
    if let Some(hash) = axes.access_shape {
        push_axis_key(&mut keys, param_count, AxisKind::AccessShape, hash);
    }
    if let Some(hash) = axes.callee_set {
        push_axis_key(&mut keys, param_count, AxisKind::CalleeSet, hash);
    }
    if let Some(hash) = axes.throw_set {
        push_axis_key(&mut keys, param_count, AxisKind::ThrowSet, hash);
    }
    keys
}

fn push_axis_key(keys: &mut BTreeSet<AxisKey>, param_count: u32, kind: AxisKind, hash: u64) {
    // Empty hashes are valid values from the axis calculators but tend to be
    // common on tiny functions. Frequency filtering removes the worst cases;
    // keeping them here lets aggregate counts still help when all functions in
    // a package share a deliberately tiny but repeated implementation shape.
    keys.insert(AxisKey {
        param_count,
        kind,
        hash,
    });
}

fn candidate_package_bags<'a>(
    package_bags: &'a [PackageStructuralBag],
    module_bag: &ModuleStructuralBag,
) -> Vec<&'a PackageStructuralBag> {
    let Some(package_name) = module_bag.package_name.as_deref() else {
        return Vec::new();
    };
    let package_bags_for_name = package_bags
        .iter()
        .filter(|bag| bag.package_name == package_name)
        .collect::<Vec<_>>();
    let Some(version_hint) = module_bag
        .package_version
        .as_deref()
        .map(str::trim)
        .filter(|version| !version.is_empty())
        .filter(|version| Version::parse(version).is_ok())
    else {
        return package_bags_for_name;
    };
    let exact = package_bags_for_name
        .iter()
        .copied()
        .filter(|bag| bag.package_version == version_hint)
        .collect::<Vec<_>>();
    if exact.is_empty() {
        package_bags_for_name
    } else {
        exact
    }
}

fn best_package_score<'a>(
    module_bag: &ModuleStructuralBag,
    candidates: &[&'a PackageStructuralBag],
) -> Option<PackageScore<'a>> {
    let mut scores = candidates
        .iter()
        .filter_map(|candidate| score_candidate(module_bag, candidate))
        .collect::<Vec<_>>();
    scores.sort_by(|left, right| compare_scores(right, left));
    let mut best = scores.into_iter().next()?;
    let runner_up = candidates
        .iter()
        .filter(|candidate| {
            candidate.package_name != best.bag.package_name
                || candidate.package_version != best.bag.package_version
        })
        .filter_map(|candidate| score_candidate(module_bag, candidate))
        .map(|score| score.score)
        .fold(0.0_f64, f64::max);
    best.margin = if best.score <= f64::EPSILON {
        0.0
    } else {
        ((best.score - runner_up) / best.score).clamp(0.0, 1.0)
    };
    accept_score(module_bag, &best).then_some(best)
}

fn compare_scores(left: &PackageScore<'_>, right: &PackageScore<'_>) -> Ordering {
    left.score
        .partial_cmp(&right.score)
        .unwrap_or(Ordering::Equal)
        .then_with(|| left.matched_weight.cmp(&right.matched_weight))
        .then_with(|| left.bag.package_version.cmp(&right.bag.package_version))
}

fn score_candidate<'a>(
    module_bag: &ModuleStructuralBag,
    package_bag: &'a PackageStructuralBag,
) -> Option<PackageScore<'a>> {
    if module_bag.total_weight == 0 || package_bag.total_weight == 0 {
        return None;
    }

    let mut matched_weight = 0u32;
    let mut matched_strong_axes = 0usize;
    for (key, module_count) in &module_bag.axis_counts {
        if !is_distinctive_package_key(package_bag, key) {
            continue;
        }
        let Some(package_count) = package_bag.axis_counts.get(key) else {
            continue;
        };
        let matched_count = (*module_count).min(*package_count);
        matched_weight = matched_weight
            .saturating_add(axis_weight(key.kind).saturating_mul(matched_count as u32));
        if is_strong_axis(key.kind) {
            matched_strong_axes += matched_count;
        }
    }

    if matched_weight == 0 {
        return None;
    }

    let matched_functions = module_bag
        .functions
        .iter()
        .filter(|function| function_matches_package(function, package_bag))
        .count();
    let coverage = f64::from(matched_weight) / f64::from(module_bag.total_weight);
    let function_coverage = matched_functions as f64 / module_bag.functions.len() as f64;
    let score = coverage.mul_add(1_000.0, function_coverage * 200.0)
        + matched_strong_axes.min(25) as f64 * 4.0;

    Some(PackageScore {
        bag: package_bag,
        matched_weight,
        module_weight: module_bag.total_weight,
        coverage,
        function_coverage,
        margin: 0.0,
        score,
        matched_strong_axes,
        matched_functions,
    })
}

fn function_matches_package(
    function: &FunctionStructuralBag,
    package_bag: &PackageStructuralBag,
) -> bool {
    let mut strong_hits = 0usize;
    let mut any_hit = false;
    for key in &function.keys {
        if !is_distinctive_package_key(package_bag, key)
            || !package_bag.axis_counts.contains_key(key)
        {
            continue;
        }
        any_hit = true;
        if is_strong_axis(key.kind) {
            strong_hits += 1;
        }
    }
    strong_hits >= 1 || any_hit && function.keys.len() <= 3
}

fn accept_score(module_bag: &ModuleStructuralBag, score: &PackageScore<'_>) -> bool {
    if score.matched_functions == 0 || score.matched_strong_axes < 2 {
        return false;
    }
    if module_bag.functions.len() == 1 {
        return score.coverage >= 0.75 && score.matched_strong_axes >= 3 && score.margin >= 0.50;
    }
    score.coverage >= 0.35 && score.function_coverage >= 0.50 && score.margin >= 0.30
}

fn is_distinctive_package_key(package_bag: &PackageStructuralBag, key: &AxisKey) -> bool {
    let frequency = package_bag
        .axis_function_counts
        .get(key)
        .copied()
        .unwrap_or_default();
    if frequency == 0 || package_bag.function_count < 8 {
        return true;
    }
    let limit = (package_bag.function_count.div_ceil(5)).clamp(3, 50);
    frequency <= limit
}

const fn is_strong_axis(kind: AxisKind) -> bool {
    matches!(
        kind,
        AxisKind::Ast
            | AxisKind::Cfg
            | AxisKind::LiteralAnchor
            | AxisKind::AccessPattern
            | AxisKind::StructuralAnchor
            | AxisKind::AccessShape
            | AxisKind::CalleeSet
            | AxisKind::ThrowSet
    )
}

const fn axis_weight(kind: AxisKind) -> u32 {
    match kind {
        AxisKind::Ast => 4,
        AxisKind::Cfg
        | AxisKind::StructuralAnchor
        | AxisKind::LiteralAnchor
        | AxisKind::ThrowSet => 3,
        AxisKind::AccessPattern | AxisKind::AccessShape | AxisKind::CalleeSet => 2,
        AxisKind::ReturnPattern
        | AxisKind::EffectPattern
        | AxisKind::LiteralShape
        | AxisKind::BindingPattern => 1,
    }
}

fn structural_bag_source_path(score: &PackageScore<'_>) -> String {
    format!(
        "structural-bag:{}@{}:score={:.3}:coverage={:.3}:function_coverage={:.3}:margin={:.3}:weight={}/{}:strong_axes={}",
        score.bag.package_name,
        score.bag.package_version,
        score.score,
        score.coverage,
        score.function_coverage,
        score.margin,
        score.matched_weight,
        score.module_weight,
        score.matched_strong_axes,
    )
}
