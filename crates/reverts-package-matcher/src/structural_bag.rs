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
    match_structural_bags_with_excluded_modules(
        rows,
        package_sources,
        package_filter,
        &BTreeSet::new(),
    )
}

/// Like [`match_structural_bags`], but skips modules that have already been
/// matched by stronger package strategies in the caller's current report.
#[must_use]
pub fn match_structural_bags_with_excluded_modules(
    rows: &InputRows,
    package_sources: &[PackageSource],
    package_filter: Option<&BTreeSet<String>>,
    excluded_modules: &BTreeSet<ModuleId>,
) -> StructuralBagMatchReport {
    let mut audit = AuditReport::default();
    let module_bags = candidate_modules(rows, package_filter, excluded_modules)
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
    excluded_modules: &'a BTreeSet<ModuleId>,
) -> impl Iterator<Item = &'a ModuleInput> + 'a {
    rows.modules.iter().filter(move |module| {
        module.kind == ModuleKind::Package
            && !excluded_modules.contains(&module.id)
            && !has_accepted_external_attribution(rows, module.id)
            && package_filter.is_none_or(|filter| {
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
    pair_counts: BTreeMap<AxisPairKey, usize>,
    shape_counts: BTreeMap<FunctionShapeKey, usize>,
    total_weight: u32,
    total_pair_weight: u32,
    total_shapes: usize,
    functions: Vec<FunctionStructuralBag>,
}

#[derive(Debug, Clone)]
struct PackageStructuralBag {
    package_name: String,
    package_version: String,
    axis_counts: BTreeMap<AxisKey, usize>,
    axis_function_counts: BTreeMap<AxisKey, usize>,
    pair_counts: BTreeMap<AxisPairKey, usize>,
    pair_function_counts: BTreeMap<AxisPairKey, usize>,
    shape_counts: BTreeMap<FunctionShapeKey, usize>,
    shape_function_counts: BTreeMap<FunctionShapeKey, usize>,
    total_weight: u32,
    function_count: usize,
}

#[derive(Debug, Clone)]
struct FunctionStructuralBag {
    keys: BTreeSet<AxisKey>,
    pairs: BTreeSet<AxisPairKey>,
    shapes: BTreeSet<FunctionShapeKey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct AxisKey {
    param_count: u32,
    kind: AxisKind,
    hash: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct AxisPairKey {
    param_count: u32,
    left_kind: AxisKind,
    left_hash: u64,
    right_kind: AxisKind,
    right_hash: u64,
}

/// Coarse per-function shape built from axes that are resilient to local
/// identifier renaming and most minifier formatting changes.
///
/// It intentionally excludes exact AST and literal/callee anchors. Those are
/// already represented by individual axis keys; the shape key is the coarse key
/// for real bundles where individual low-cardinality axes are too common but
/// their combination is still distinctive for a package version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct FunctionShapeKey {
    param_count: u32,
    statement_count: u32,
    cfg: u64,
    return_pattern: u64,
    effect_pattern: u64,
    structural_anchor: u64,
    binding_pattern: u64,
    literal_shape: Option<u64>,
    access_shape: Option<u64>,
    throw_set: Option<u64>,
}

#[derive(Debug, Clone)]
struct PackageScore<'a> {
    bag: &'a PackageStructuralBag,
    matched_weight: u32,
    module_weight: u32,
    matched_pair_weight: u32,
    module_pair_weight: u32,
    coverage: f64,
    pair_coverage: f64,
    shape_coverage: f64,
    function_coverage: f64,
    pair_function_coverage: f64,
    shape_function_coverage: f64,
    margin: f64,
    score: f64,
    matched_strong_axes: usize,
    matched_pairs: usize,
    matched_shapes: usize,
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
    let (pair_counts, total_pair_weight) = pair_counts_for_functions(&functions);
    let (shape_counts, total_shapes) = shape_counts_for_functions(&functions);
    Some(ModuleStructuralBag {
        module_id: module.id,
        package_name: module.package_name.clone(),
        package_version: module.package_version.clone(),
        axis_counts,
        pair_counts,
        shape_counts,
        total_weight,
        total_pair_weight,
        total_shapes,
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
            let (pair_counts, _) = pair_counts_for_functions(&functions);
            let (shape_counts, _) = shape_counts_for_functions(&functions);
            let mut axis_function_counts = BTreeMap::<AxisKey, usize>::new();
            let mut pair_function_counts = BTreeMap::<AxisPairKey, usize>::new();
            let mut shape_function_counts = BTreeMap::<FunctionShapeKey, usize>::new();
            for function in &functions {
                for key in &function.keys {
                    *axis_function_counts.entry(*key).or_default() += 1;
                }
                for key in &function.pairs {
                    *pair_function_counts.entry(*key).or_default() += 1;
                }
                for key in &function.shapes {
                    *shape_function_counts.entry(*key).or_default() += 1;
                }
            }
            Some(PackageStructuralBag {
                package_name,
                package_version,
                axis_counts,
                axis_function_counts,
                pair_counts,
                pair_function_counts,
                shape_counts,
                shape_function_counts,
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
            let mut pairs = axis_pair_keys_for_axes(fingerprint.param_count, &fingerprint.primary);
            let mut shapes = BTreeSet::from([function_shape_key_for_axes(
                fingerprint.param_count,
                fingerprint.statement_count,
                &fingerprint.primary,
            )]);
            for alternate in &fingerprint.alternates {
                keys.extend(axis_keys_for_axes(fingerprint.param_count, &alternate.axes));
                pairs.extend(axis_pair_keys_for_axes(
                    fingerprint.param_count,
                    &alternate.axes,
                ));
                shapes.insert(function_shape_key_for_axes(
                    fingerprint.param_count,
                    alternate.statement_count,
                    &alternate.axes,
                ));
            }
            if keys.is_empty() {
                None
            } else {
                Some(FunctionStructuralBag {
                    keys,
                    pairs,
                    shapes,
                })
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

fn pair_counts_for_functions(
    functions: &[FunctionStructuralBag],
) -> (BTreeMap<AxisPairKey, usize>, u32) {
    let mut pair_counts = BTreeMap::<AxisPairKey, usize>::new();
    let mut total_weight = 0u32;
    for function in functions {
        for key in &function.pairs {
            *pair_counts.entry(*key).or_default() += 1;
            total_weight = total_weight.saturating_add(axis_pair_weight(key));
        }
    }
    (pair_counts, total_weight)
}

fn shape_counts_for_functions(
    functions: &[FunctionStructuralBag],
) -> (BTreeMap<FunctionShapeKey, usize>, usize) {
    let mut shape_counts = BTreeMap::<FunctionShapeKey, usize>::new();
    let mut total_shapes = 0usize;
    for function in functions {
        for key in &function.shapes {
            *shape_counts.entry(*key).or_default() += 1;
            total_shapes = total_shapes.saturating_add(1);
        }
    }
    (shape_counts, total_shapes)
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

fn axis_pair_keys_for_axes(param_count: u32, axes: &AxisHashes) -> BTreeSet<AxisPairKey> {
    const PAIR_KINDS: &[(AxisKind, AxisKind)] = &[
        (AxisKind::Cfg, AxisKind::StructuralAnchor),
        (AxisKind::Cfg, AxisKind::ReturnPattern),
        (AxisKind::Cfg, AxisKind::EffectPattern),
        (AxisKind::StructuralAnchor, AxisKind::ReturnPattern),
        (AxisKind::StructuralAnchor, AxisKind::EffectPattern),
        (AxisKind::StructuralAnchor, AxisKind::BindingPattern),
        (AxisKind::ReturnPattern, AxisKind::EffectPattern),
        (AxisKind::ReturnPattern, AxisKind::BindingPattern),
        (AxisKind::LiteralShape, AxisKind::AccessShape),
        (AxisKind::LiteralShape, AxisKind::ReturnPattern),
        (AxisKind::AccessShape, AxisKind::EffectPattern),
        (AxisKind::ThrowSet, AxisKind::StructuralAnchor),
    ];
    PAIR_KINDS
        .iter()
        .filter_map(|(left, right)| axis_pair_key_for_axes(param_count, *left, *right, axes))
        .collect()
}

fn axis_pair_key_for_axes(
    param_count: u32,
    left_kind: AxisKind,
    right_kind: AxisKind,
    axes: &AxisHashes,
) -> Option<AxisPairKey> {
    let left_hash = axes.get(left_kind)?;
    let right_hash = axes.get(right_kind)?;
    let (left_kind, left_hash, right_kind, right_hash) = if left_kind <= right_kind {
        (left_kind, left_hash, right_kind, right_hash)
    } else {
        (right_kind, right_hash, left_kind, left_hash)
    };
    Some(AxisPairKey {
        param_count,
        left_kind,
        left_hash,
        right_kind,
        right_hash,
    })
}

fn function_shape_key_for_axes(
    param_count: u32,
    statement_count: u32,
    axes: &AxisHashes,
) -> FunctionShapeKey {
    FunctionShapeKey {
        param_count,
        statement_count,
        cfg: axes.cfg,
        return_pattern: axes.return_pattern,
        effect_pattern: axes.effect_pattern,
        structural_anchor: axes.structural_anchor,
        binding_pattern: axes.binding_pattern,
        literal_shape: axes.literal_shape,
        access_shape: axes.access_shape,
        throw_set: axes.throw_set,
    }
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
        .then_with(|| left.matched_pair_weight.cmp(&right.matched_pair_weight))
        .then_with(|| left.matched_shapes.cmp(&right.matched_shapes))
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

    let mut matched_pair_weight = 0u32;
    let mut matched_pairs = 0usize;
    for (key, module_count) in &module_bag.pair_counts {
        if !is_distinctive_package_pair(package_bag, key) {
            continue;
        }
        let Some(package_count) = package_bag.pair_counts.get(key) else {
            continue;
        };
        let matched_count = (*module_count).min(*package_count);
        matched_pairs += matched_count;
        matched_pair_weight = matched_pair_weight
            .saturating_add(axis_pair_weight(key).saturating_mul(matched_count as u32));
    }

    let mut matched_shapes = 0usize;
    for (key, module_count) in &module_bag.shape_counts {
        if !is_distinctive_package_shape(package_bag, key) {
            continue;
        }
        let Some(package_count) = package_bag.shape_counts.get(key) else {
            continue;
        };
        matched_shapes += (*module_count).min(*package_count);
    }

    if matched_weight == 0 && matched_pair_weight == 0 && matched_shapes == 0 {
        return None;
    }

    let matched_functions = module_bag
        .functions
        .iter()
        .filter(|function| function_matches_package(function, package_bag))
        .count();
    let pair_matched_functions = module_bag
        .functions
        .iter()
        .filter(|function| function_pair_matches_package(function, package_bag))
        .count();
    let shape_matched_functions = module_bag
        .functions
        .iter()
        .filter(|function| function_shape_matches_package(function, package_bag))
        .count();
    let coverage = f64::from(matched_weight) / f64::from(module_bag.total_weight);
    let pair_coverage = if module_bag.total_pair_weight == 0 {
        0.0
    } else {
        f64::from(matched_pair_weight) / f64::from(module_bag.total_pair_weight)
    };
    let shape_coverage = if module_bag.total_shapes == 0 {
        0.0
    } else {
        matched_shapes as f64 / module_bag.total_shapes as f64
    };
    let function_coverage = matched_functions as f64 / module_bag.functions.len() as f64;
    let pair_function_coverage = pair_matched_functions as f64 / module_bag.functions.len() as f64;
    let shape_function_coverage =
        shape_matched_functions as f64 / module_bag.functions.len() as f64;
    let score = coverage.mul_add(900.0, pair_coverage * 450.0)
        + shape_coverage * 180.0
        + function_coverage * 150.0
        + pair_function_coverage * 220.0
        + shape_function_coverage * 300.0
        + matched_strong_axes.min(25) as f64 * 4.0
        + matched_pairs.min(25) as f64 * 5.0
        + matched_shapes.min(25) as f64 * 6.0;

    Some(PackageScore {
        bag: package_bag,
        matched_weight,
        module_weight: module_bag.total_weight,
        matched_pair_weight,
        module_pair_weight: module_bag.total_pair_weight,
        coverage,
        pair_coverage,
        shape_coverage,
        function_coverage,
        pair_function_coverage,
        shape_function_coverage,
        margin: 0.0,
        score,
        matched_strong_axes,
        matched_pairs,
        matched_shapes,
        matched_functions,
    })
}

fn function_matches_package(
    function: &FunctionStructuralBag,
    package_bag: &PackageStructuralBag,
) -> bool {
    function_axis_matches_package(function, package_bag)
        || function_pair_matches_package(function, package_bag)
        || function_shape_matches_package(function, package_bag)
}

fn function_axis_matches_package(
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

fn function_pair_matches_package(
    function: &FunctionStructuralBag,
    package_bag: &PackageStructuralBag,
) -> bool {
    function.pairs.iter().any(|key| {
        is_distinctive_package_pair(package_bag, key) && package_bag.pair_counts.contains_key(key)
    })
}

fn function_shape_matches_package(
    function: &FunctionStructuralBag,
    package_bag: &PackageStructuralBag,
) -> bool {
    function.shapes.iter().any(|key| {
        is_distinctive_package_shape(package_bag, key) && package_bag.shape_counts.contains_key(key)
    })
}

fn accept_score(module_bag: &ModuleStructuralBag, score: &PackageScore<'_>) -> bool {
    let structured_hits = score.matched_strong_axes + score.matched_pairs + score.matched_shapes;
    if score.matched_functions == 0 || structured_hits < 2 {
        return false;
    }
    if module_bag.functions.len() == 1 {
        return score.coverage >= 0.75 && score.matched_strong_axes >= 3 && score.margin >= 0.50;
    }
    let axis_supported =
        score.coverage >= 0.35 && score.function_coverage >= 0.50 && score.matched_strong_axes >= 2;
    let pair_supported = score.pair_coverage >= 0.35
        && score.pair_function_coverage >= 0.50
        && score.matched_pairs >= 2;
    let shape_supported = score.shape_coverage >= 0.25
        && score.shape_function_coverage >= 0.60
        && score.matched_shapes >= 2
        && (score.matched_pairs >= 1
            || score.matched_strong_axes >= 1
            || score.coverage >= 0.15
            || score.pair_coverage >= 0.15);
    score.margin >= 0.30 && (axis_supported || pair_supported || shape_supported)
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

fn is_distinctive_package_pair(package_bag: &PackageStructuralBag, key: &AxisPairKey) -> bool {
    let frequency = package_bag
        .pair_function_counts
        .get(key)
        .copied()
        .unwrap_or_default();
    if frequency == 0 || package_bag.function_count < 8 {
        return true;
    }
    let limit = (package_bag.function_count.div_ceil(4)).clamp(2, 40);
    frequency <= limit
}

fn is_distinctive_package_shape(
    package_bag: &PackageStructuralBag,
    key: &FunctionShapeKey,
) -> bool {
    let frequency = package_bag
        .shape_function_counts
        .get(key)
        .copied()
        .unwrap_or_default();
    if frequency == 0 || package_bag.function_count < 8 {
        return true;
    }
    let limit = (package_bag.function_count.div_ceil(4)).clamp(2, 40);
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

const fn axis_pair_weight(key: &AxisPairKey) -> u32 {
    match (key.left_kind, key.right_kind) {
        (AxisKind::Cfg, AxisKind::StructuralAnchor) => 5,
        (AxisKind::Cfg, _)
        | (_, AxisKind::Cfg)
        | (AxisKind::StructuralAnchor, _)
        | (_, AxisKind::StructuralAnchor) => 4,
        (AxisKind::LiteralShape, AxisKind::AccessShape)
        | (AxisKind::ReturnPattern, AxisKind::EffectPattern) => 3,
        _ => 2,
    }
}

fn structural_bag_source_path(score: &PackageScore<'_>) -> String {
    format!(
        "structural-bag:{}@{}:score={:.3}:coverage={:.3}:pair_coverage={:.3}:shape_coverage={:.3}:function_coverage={:.3}:pair_function_coverage={:.3}:shape_function_coverage={:.3}:margin={:.3}:weight={}/{}:pair_weight={}/{}:strong_axes={}:pairs={}:shapes={}",
        score.bag.package_name,
        score.bag.package_version,
        score.score,
        score.coverage,
        score.pair_coverage,
        score.shape_coverage,
        score.function_coverage,
        score.pair_function_coverage,
        score.shape_function_coverage,
        score.margin,
        score.matched_weight,
        score.module_weight,
        score.matched_pair_weight,
        score.module_pair_weight,
        score.matched_strong_axes,
        score.matched_pairs,
        score.matched_shapes,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use reverts_ir::{AxisKind, ModuleId};

    use super::*;

    fn axis(kind: AxisKind, hash: u64) -> AxisKey {
        AxisKey {
            param_count: 1,
            kind,
            hash,
        }
    }

    fn pair(
        left_kind: AxisKind,
        left_hash: u64,
        right_kind: AxisKind,
        right_hash: u64,
    ) -> AxisPairKey {
        let (left_kind, left_hash, right_kind, right_hash) = if left_kind <= right_kind {
            (left_kind, left_hash, right_kind, right_hash)
        } else {
            (right_kind, right_hash, left_kind, left_hash)
        };
        AxisPairKey {
            param_count: 1,
            left_kind,
            left_hash,
            right_kind,
            right_hash,
        }
    }

    fn shape(seed: u64) -> FunctionShapeKey {
        FunctionShapeKey {
            param_count: 1,
            statement_count: 2,
            cfg: 10 + seed,
            return_pattern: 20 + seed,
            effect_pattern: 30 + seed,
            structural_anchor: 40 + seed,
            binding_pattern: 50 + seed,
            literal_shape: Some(60 + seed),
            access_shape: Some(70 + seed),
            throw_set: None,
        }
    }

    fn counts<K: Copy + Ord>(entries: &[(K, usize)]) -> BTreeMap<K, usize> {
        entries.iter().copied().collect()
    }

    fn function(
        keys: impl IntoIterator<Item = AxisKey>,
        pairs: impl IntoIterator<Item = AxisPairKey>,
        shapes: impl IntoIterator<Item = FunctionShapeKey>,
    ) -> FunctionStructuralBag {
        FunctionStructuralBag {
            keys: keys.into_iter().collect(),
            pairs: pairs.into_iter().collect(),
            shapes: shapes.into_iter().collect(),
        }
    }

    fn module_bag(functions: Vec<FunctionStructuralBag>) -> ModuleStructuralBag {
        let (axis_counts, total_weight) = axis_counts_for_functions(&functions);
        let (pair_counts, total_pair_weight) = pair_counts_for_functions(&functions);
        let (shape_counts, total_shapes) = shape_counts_for_functions(&functions);
        ModuleStructuralBag {
            module_id: ModuleId(10),
            package_name: Some("pkg".to_string()),
            package_version: None,
            axis_counts,
            pair_counts,
            shape_counts,
            total_weight,
            total_pair_weight,
            total_shapes,
            functions,
        }
    }

    fn package_bag(
        axis_counts: BTreeMap<AxisKey, usize>,
        axis_function_counts: BTreeMap<AxisKey, usize>,
        pair_counts: BTreeMap<AxisPairKey, usize>,
        pair_function_counts: BTreeMap<AxisPairKey, usize>,
        shape_counts: BTreeMap<FunctionShapeKey, usize>,
        shape_function_counts: BTreeMap<FunctionShapeKey, usize>,
        function_count: usize,
    ) -> PackageStructuralBag {
        let total_weight = axis_counts.iter().fold(0u32, |total, (key, count)| {
            total.saturating_add(axis_weight(key.kind).saturating_mul(*count as u32))
        });
        PackageStructuralBag {
            package_name: "pkg".to_string(),
            package_version: "1.2.3".to_string(),
            axis_counts,
            axis_function_counts,
            pair_counts,
            pair_function_counts,
            shape_counts,
            shape_function_counts,
            total_weight,
            function_count,
        }
    }

    #[test]
    fn accepts_distinctive_pairs_when_individual_axes_are_too_common() {
        let cfg = axis(AxisKind::Cfg, 1);
        let structural = axis(AxisKind::StructuralAnchor, 2);
        let cfg_structural = pair(AxisKind::Cfg, 1, AxisKind::StructuralAnchor, 2);
        let module = module_bag(vec![
            function([cfg, structural], [cfg_structural], []),
            function([cfg, structural], [cfg_structural], []),
        ]);
        let package = package_bag(
            counts(&[(cfg, 8), (structural, 8)]),
            counts(&[(cfg, 8), (structural, 8)]),
            counts(&[(cfg_structural, 2)]),
            counts(&[(cfg_structural, 2)]),
            BTreeMap::new(),
            BTreeMap::new(),
            8,
        );

        let score =
            best_package_score(&module, &[&package]).expect("distinctive pair should match");

        assert_eq!(
            score.matched_weight, 0,
            "common individual axes must be filtered out"
        );
        assert!(score.matched_pair_weight > 0);
        assert_eq!(score.matched_pairs, 2);
        assert_eq!(score.matched_functions, 2);
        assert!(structural_bag_source_path(&score).contains("pair_coverage="));
    }

    #[test]
    fn accepts_distinctive_function_shapes_with_minimal_pair_support() {
        let common_cfg = axis(AxisKind::Cfg, 100);
        let matched_pair = pair(AxisKind::Cfg, 101, AxisKind::StructuralAnchor, 102);
        let first_shape = shape(1);
        let second_shape = shape(2);
        let third_shape = shape(3);
        let module = module_bag(vec![
            function([common_cfg], [matched_pair], [first_shape]),
            function([common_cfg], BTreeSet::new(), [second_shape]),
            function([common_cfg], BTreeSet::new(), [third_shape]),
        ]);
        let package = package_bag(
            counts(&[(common_cfg, 8)]),
            counts(&[(common_cfg, 8)]),
            counts(&[(matched_pair, 1)]),
            counts(&[(matched_pair, 1)]),
            counts(&[(first_shape, 1), (second_shape, 1), (third_shape, 1)]),
            counts(&[(first_shape, 1), (second_shape, 1), (third_shape, 1)]),
            8,
        );

        let score = best_package_score(&module, &[&package])
            .expect("distinctive multi-function shapes should match");

        assert_eq!(
            score.matched_weight, 0,
            "shape-only matching should not rely on common individual axes"
        );
        assert_eq!(score.matched_pairs, 1);
        assert_eq!(score.matched_shapes, 3);
        assert_eq!(score.matched_functions, 3);
        assert!(score.pair_function_coverage < 0.50);
        assert!(score.shape_function_coverage >= 1.0);
        assert!(structural_bag_source_path(&score).contains("shape_function_coverage="));
    }

    #[test]
    fn rejects_single_function_shape_only_matches() {
        let common_cfg = axis(AxisKind::Cfg, 100);
        let matched_pair = pair(AxisKind::Cfg, 101, AxisKind::StructuralAnchor, 102);
        let matched_shape = shape(1);
        let module = module_bag(vec![function(
            [common_cfg],
            [matched_pair],
            [matched_shape],
        )]);
        let package = package_bag(
            counts(&[(common_cfg, 8)]),
            counts(&[(common_cfg, 8)]),
            counts(&[(matched_pair, 1)]),
            counts(&[(matched_pair, 1)]),
            counts(&[(matched_shape, 1)]),
            counts(&[(matched_shape, 1)]),
            8,
        );

        assert!(
            best_package_score(&module, &[&package]).is_none(),
            "single-function wrappers are too risky for shape-only ownership"
        );
    }
}
