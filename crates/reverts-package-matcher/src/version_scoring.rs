//! Per-package-version scoring: combines per-module fingerprint matches into
//! a [`ScoredPackageVersion`] and disambiguates exact source-hash hits.
//! Pure functions over [`PackageVersionCandidate`] / [`ModuleMatchFingerprint`]
//! inputs; no I/O.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use reverts_input::PackageAttributionInput;
use reverts_ir::split_bare_specifier;
use semver::Version;

use crate::index::ScoredPackageVersion;
use crate::package_helpers::is_json_source_path;
use crate::{
    ModuleMatchFingerprint, ModuleMatchStrategy, ModulePackageMatch, PackageSourceFingerprint,
    PackageVersionCandidate, VersionMatchScore, VersionedPackageMatcherConfig,
};

const SOURCE_HASH_WEIGHT: u32 = 10_000;
const MODULE_MATCH_WEIGHT: u32 = 1_000;
const FUNCTION_SIGNATURE_WEIGHT: u32 = 10;
const STRING_ANCHOR_WEIGHT: u32 = 1;

pub(crate) fn score_version<'a>(
    version: &PackageVersionCandidate<'a>,
    module_fingerprints: &[ModuleMatchFingerprint],
    config: &VersionedPackageMatcherConfig,
) -> ScoredPackageVersion {
    let mut module_matches = Vec::new();
    for module in module_fingerprints {
        if let Some(module_match) = best_source_match(version, module, config) {
            module_matches.push(module_match);
        }
    }

    let source_hash_matches = module_matches
        .iter()
        .filter(|module_match| module_match.strategy == ModuleMatchStrategy::NormalizedSourceHash)
        .count();
    let function_signature_matches = module_matches
        .iter()
        .map(|module_match| module_match.function_signature_matches)
        .sum::<usize>();
    let string_anchor_matches = module_matches
        .iter()
        .map(|module_match| module_match.string_anchor_matches)
        .sum::<usize>();
    let score = weighted_score(
        source_hash_matches,
        module_matches.len(),
        function_signature_matches,
        string_anchor_matches,
    );

    ScoredPackageVersion {
        score: VersionMatchScore {
            package_name: version.package_name.clone(),
            package_version: version.package_version.clone(),
            total_modules: module_fingerprints.len(),
            matched_modules: module_matches.len(),
            source_hash_matches,
            function_signature_matches,
            string_anchor_matches,
            score,
            binary_search_probes: 0,
        },
        module_matches,
    }
}

pub(crate) fn best_source_match(
    version: &PackageVersionCandidate<'_>,
    module: &ModuleMatchFingerprint,
    config: &VersionedPackageMatcherConfig,
) -> Option<ModulePackageMatch> {
    let exact_candidates = version
        .sources
        .iter()
        .filter(|source| {
            !source
                .normalized_source_hashes
                .is_disjoint(&module.normalized_source_hashes)
        })
        .collect::<Vec<_>>();
    if !exact_candidates.is_empty() {
        if let Some(selection) = disambiguate_exact_source_candidate(exact_candidates.as_slice()) {
            return Some(module_package_match(
                module,
                selection.source,
                ModuleMatchStrategy::NormalizedSourceHash,
                selection
                    .source
                    .function_signature_hashes
                    .intersection(&module.function_signature_hashes)
                    .count(),
                selection
                    .source
                    .string_anchors
                    .intersection(&module.string_anchors)
                    .count(),
                selection.external_importable,
            ));
        }
        return None;
    }

    let mut ranked = version
        .sources
        .iter()
        .filter_map(|source| {
            let function_signature_matches = source
                .function_signature_hashes
                .intersection(&module.function_signature_hashes)
                .count();
            let string_anchor_matches = source
                .string_anchors
                .intersection(&module.string_anchors)
                .count();
            let property_shape_matches =
                property_shape_anchor_matches(&source.string_anchors, &module.string_anchors);
            let property_anchor_matches =
                property_anchor_matches(&source.string_anchors, &module.string_anchors);
            let object_shape_matches =
                object_shape_anchor_matches(&source.string_anchors, &module.string_anchors);
            let object_anchor_matches =
                object_anchor_matches(&source.string_anchors, &module.string_anchors);
            let class_shape_matches =
                class_shape_anchor_matches(&source.string_anchors, &module.string_anchors);
            let class_anchor_matches =
                class_anchor_matches(&source.string_anchors, &module.string_anchors);
            let switch_shape_matches =
                switch_shape_anchor_matches(&source.string_anchors, &module.string_anchors);
            let switch_anchor_matches =
                switch_anchor_matches(&source.string_anchors, &module.string_anchors);
            let is_function_string_match = function_signature_matches
                >= config.min_function_signature_matches
                && string_anchor_matches >= config.min_string_anchor_matches;
            let is_property_shape_match = property_shape_matches >= 1 && string_anchor_matches >= 4;
            let is_object_shape_match = object_shape_matches >= 1 && string_anchor_matches >= 5;
            let is_class_shape_match = class_shape_matches >= 1 && string_anchor_matches >= 4;
            let is_switch_shape_match = switch_shape_matches >= 1 && string_anchor_matches >= 4;
            if is_function_string_match
                || is_property_shape_match
                || is_object_shape_match
                || is_class_shape_match
                || is_switch_shape_match
            {
                Some((
                    source,
                    function_signature_matches
                        .max(property_anchor_matches)
                        .max(object_anchor_matches)
                        .max(class_anchor_matches)
                        .max(switch_anchor_matches),
                    string_anchor_matches,
                    if is_function_string_match {
                        ModuleMatchStrategy::FunctionSignatureAndStringAnchors
                    } else if is_property_shape_match {
                        ModuleMatchStrategy::PropertyShapeAndStringAnchors
                    } else if is_object_shape_match {
                        ModuleMatchStrategy::ObjectShapeAndStringAnchors
                    } else if is_class_shape_match {
                        ModuleMatchStrategy::ClassShapeAndStringAnchors
                    } else {
                        ModuleMatchStrategy::SwitchShapeAndStringAnchors
                    },
                ))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| right.2.cmp(&left.2))
            .then_with(|| {
                right
                    .0
                    .source
                    .external_importable
                    .cmp(&left.0.source.external_importable)
            })
            .then_with(|| left.3.cmp(&right.3))
            .then_with(|| left.0.source.source_path.cmp(&right.0.source.source_path))
    });

    let Some(best) = ranked.first() else {
        return best_aggregate_match(version, module, config);
    };
    if ranked
        .get(1)
        .is_some_and(|next| next.1 == best.1 && next.2 == best.2 && next.3 == best.3)
    {
        return best_aggregate_match(version, module, config);
    }

    Some(module_package_match(
        module,
        best.0,
        best.3,
        best.1,
        best.2,
        best.0.source.external_importable,
    ))
}

fn property_shape_anchor_matches(left: &BTreeSet<String>, right: &BTreeSet<String>) -> usize {
    left.intersection(right)
        .filter(|anchor| anchor.starts_with("prototype-shape:"))
        .count()
}

fn property_anchor_matches(left: &BTreeSet<String>, right: &BTreeSet<String>) -> usize {
    left.intersection(right)
        .filter(|anchor| {
            anchor.starts_with("prototype-shape:") || anchor.starts_with("prototype-member:")
        })
        .count()
}

fn object_shape_anchor_matches(left: &BTreeSet<String>, right: &BTreeSet<String>) -> usize {
    left.intersection(right)
        .filter(|anchor| anchor.starts_with("object-shape:"))
        .count()
}

fn object_anchor_matches(left: &BTreeSet<String>, right: &BTreeSet<String>) -> usize {
    left.intersection(right)
        .filter(|anchor| anchor.starts_with("object-shape:") || anchor.starts_with("object-key:"))
        .count()
}

fn class_shape_anchor_matches(left: &BTreeSet<String>, right: &BTreeSet<String>) -> usize {
    left.intersection(right)
        .filter(|anchor| anchor.starts_with("class-shape:"))
        .count()
}

fn class_anchor_matches(left: &BTreeSet<String>, right: &BTreeSet<String>) -> usize {
    left.intersection(right)
        .filter(|anchor| anchor.starts_with("class-shape:") || anchor.starts_with("class-method:"))
        .count()
}

fn switch_shape_anchor_matches(left: &BTreeSet<String>, right: &BTreeSet<String>) -> usize {
    left.intersection(right)
        .filter(|anchor| anchor.starts_with("switch-shape:"))
        .count()
}

fn switch_anchor_matches(left: &BTreeSet<String>, right: &BTreeSet<String>) -> usize {
    left.intersection(right)
        .filter(|anchor| anchor.starts_with("switch-shape:") || anchor.starts_with("switch-case:"))
        .count()
}

fn best_aggregate_match(
    version: &PackageVersionCandidate<'_>,
    module: &ModuleMatchFingerprint,
    config: &VersionedPackageMatcherConfig,
) -> Option<ModulePackageMatch> {
    let mut function_signature_hashes = BTreeSet::new();
    let mut string_anchors = BTreeSet::new();
    for source in &version.sources {
        function_signature_hashes.extend(source.function_signature_hashes.iter().cloned());
        string_anchors.extend(source.string_anchors.iter().cloned());
    }
    let function_signature_matches = function_signature_hashes
        .intersection(&module.function_signature_hashes)
        .count();
    let string_anchor_matches = string_anchors.intersection(&module.string_anchors).count();
    let min_function_matches = config.min_function_signature_matches.max(3);
    if function_signature_matches < min_function_matches
        || string_anchor_matches < config.min_string_anchor_matches
    {
        return None;
    }
    Some(module_package_aggregate_match(
        module,
        version,
        function_signature_matches,
        string_anchor_matches,
    ))
}

pub(crate) struct ExactCandidateSelection<'a> {
    pub(crate) source: &'a PackageSourceFingerprint<'a>,
    pub(crate) external_importable: bool,
}

pub(crate) fn disambiguate_exact_source_candidate<'a>(
    candidates: &[&'a PackageSourceFingerprint<'a>],
) -> Option<ExactCandidateSelection<'a>> {
    let unique_keys = candidates
        .iter()
        .map(|source| {
            (
                source.source.package_name.as_str(),
                source.source.package_version.as_str(),
                source.source.export_specifier.as_str(),
                source.source.source_path.as_str(),
            )
        })
        .collect::<BTreeSet<_>>();
    if unique_keys.len() == 1 {
        return candidates
            .first()
            .copied()
            .map(|source| ExactCandidateSelection {
                source,
                external_importable: source.source.external_importable,
            });
    }

    let package_versions = candidates
        .iter()
        .map(|source| {
            (
                source.source.package_name.as_str(),
                source.source.package_version.as_str(),
            )
        })
        .collect::<BTreeSet<_>>();
    if package_versions.len() == 1 {
        return candidates
            .first()
            .copied()
            .map(|source| ExactCandidateSelection {
                source,
                // Duplicate exact source bodies inside the same package
                // version prove ownership, but not a safe unique import
                // specifier.
                external_importable: false,
            });
    }

    None
}

pub(crate) fn module_package_match(
    module: &ModuleMatchFingerprint,
    source: &PackageSourceFingerprint<'_>,
    strategy: ModuleMatchStrategy,
    function_signature_matches: usize,
    string_anchor_matches: usize,
    external_importable: bool,
) -> ModulePackageMatch {
    let external_importable = external_importable
        && (!is_json_source_path(source.source.source_path.as_str())
            || strategy == ModuleMatchStrategy::NormalizedSourceHash);
    ModulePackageMatch {
        module_id: module.module_id,
        package_name: source.source.package_name.clone(),
        package_version: source.source.package_version.clone(),
        export_specifier: source.source.export_specifier.clone(),
        source_path: source.source.source_path.clone(),
        strategy,
        normalized_source_hash: source.normalized_source_hash.clone(),
        function_signature_matches,
        string_anchor_matches,
        external_importable,
    }
}

fn module_package_aggregate_match(
    module: &ModuleMatchFingerprint,
    version: &PackageVersionCandidate<'_>,
    function_signature_matches: usize,
    string_anchor_matches: usize,
) -> ModulePackageMatch {
    ModulePackageMatch {
        module_id: module.module_id,
        package_name: version.package_name.clone(),
        package_version: version.package_version.clone(),
        export_specifier: version.package_name.clone(),
        source_path: format!(
            "aggregate:{}@{}",
            version.package_name, version.package_version
        ),
        strategy: ModuleMatchStrategy::AggregateFunctionSignatureAndStringAnchors,
        normalized_source_hash: module.normalized_source_hash.clone(),
        function_signature_matches,
        string_anchor_matches,
        external_importable: false,
    }
}

pub(crate) fn accepted_attribution_from_match(
    module_match: &ModulePackageMatch,
) -> PackageAttributionInput {
    let mut attribution = PackageAttributionInput::accepted_external(
        module_match.module_id,
        module_match.package_name.as_str(),
        module_match.package_version.as_str(),
        module_match.export_specifier.as_str(),
    )
    .with_resolved_file(module_match.source_path.as_str());
    if let Some((_package_name, Some(subpath))) =
        split_bare_specifier(&module_match.export_specifier)
    {
        attribution = attribution.with_subpath(subpath);
    }
    attribution
}

pub(crate) fn compare_versions(left: &str, right: &str) -> Ordering {
    match (Version::parse(left), Version::parse(right)) {
        (Ok(left), Ok(right)) => left.cmp(&right),
        (Ok(_), Err(_)) => Ordering::Less,
        (Err(_), Ok(_)) => Ordering::Greater,
        (Err(_), Err(_)) => left.cmp(right),
    }
}

fn weighted_score(
    source_hash_matches: usize,
    matched_modules: usize,
    function_signature_matches: usize,
    string_anchor_matches: usize,
) -> u32 {
    (source_hash_matches as u32 * SOURCE_HASH_WEIGHT)
        + (matched_modules as u32 * MODULE_MATCH_WEIGHT)
        + (function_signature_matches as u32 * FUNCTION_SIGNATURE_WEIGHT)
        + (string_anchor_matches as u32 * STRING_ANCHOR_WEIGHT)
}
