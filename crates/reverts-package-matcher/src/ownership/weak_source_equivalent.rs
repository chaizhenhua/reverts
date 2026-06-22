//! Promote modules whose normalised source happens to be byte-identical
//! to a known package version's source even though their per-module
//! fingerprint is marked "weak". Acts as a safety net for package files
//! whose minified surface is too short to fingerprint reliably but
//! whose normalised contents still match exactly.

use std::collections::{BTreeMap, BTreeSet};

use reverts_input::InputRows;
use reverts_ir::ModuleKind;

use crate::{
    ModuleMatchStrategy, PackageMatch, PackageModuleSourceQuality, PackageSource,
    PackageSourceFingerprint, VersionedPackageMatchReport, accepted_attribution_from_match,
    accepted_external_modules, disambiguate_exact_source_candidate, is_exact_package_version_hint,
    module_match_fingerprint, module_package_match, package_module_source_quality,
    package_source_fingerprint,
};

pub(crate) fn promote_weak_source_equivalent_matches(
    rows: &InputRows,
    package_sources: &[PackageSource],
    report: &mut VersionedPackageMatchReport,
) {
    // Fingerprinting every corpus source (parse + normalize) is the dominant cost
    // of this pass; fan it out across cores. The serial fold below then assigns
    // indices and builds the lookup map in input order, so `source_index` values
    // and the map are identical to the previous single-threaded build.
    let fingerprint_results: Vec<Option<PackageSourceFingerprint<'_>>> =
        crate::par_map(package_sources, |source| {
            package_source_fingerprint(source).ok()
        });
    let mut source_fingerprints = Vec::<PackageSourceFingerprint<'_>>::new();
    let mut source_indices_by_version_hash =
        BTreeMap::<(String, String, String), Vec<usize>>::new();
    for (source, fingerprint) in package_sources.iter().zip(fingerprint_results) {
        let Some(fingerprint) = fingerprint else {
            continue;
        };
        let source_index = source_fingerprints.len();
        for hash in &fingerprint.normalized_source_hashes {
            source_indices_by_version_hash
                .entry((
                    source.package_name.clone(),
                    source.package_version.clone(),
                    hash.clone(),
                ))
                .or_default()
                .push(source_index);
        }
        source_fingerprints.push(fingerprint);
    }
    if source_fingerprints.is_empty() {
        return;
    }

    let mut already_accepted = accepted_external_modules(rows, report);
    let mut matched_modules = report
        .matches
        .iter()
        .map(|package_match| package_match.module_id)
        .collect::<BTreeSet<_>>();

    for module in &rows.modules {
        if module.kind != ModuleKind::Package
            || already_accepted.contains(&module.id)
            || matched_modules.contains(&module.id)
        {
            continue;
        }
        let Some(package_name) = module
            .package_name
            .as_deref()
            .map(str::trim)
            .filter(|package_name| !package_name.is_empty())
        else {
            continue;
        };
        let Some(package_version) = module
            .package_version
            .as_deref()
            .map(str::trim)
            .filter(|package_version| is_exact_package_version_hint(package_version))
        else {
            continue;
        };
        let Some(slice) = rows.module_source_slice(module.id) else {
            continue;
        };
        if package_module_source_quality(module, slice.source_file_path, slice.source)
            != PackageModuleSourceQuality::Weak
        {
            continue;
        }
        let Ok(module_fingerprint) =
            module_match_fingerprint(module, slice.source_file_path, slice.source)
        else {
            continue;
        };
        let candidate_indices = module_fingerprint
            .normalized_source_hashes
            .iter()
            .filter_map(|hash| {
                source_indices_by_version_hash.get(&(
                    package_name.to_string(),
                    package_version.to_string(),
                    hash.clone(),
                ))
            })
            .flat_map(|indices| indices.iter().copied())
            .collect::<BTreeSet<_>>();
        if candidate_indices.is_empty() {
            continue;
        }
        let candidates = candidate_indices
            .iter()
            .filter_map(|index| source_fingerprints.get(*index))
            .collect::<Vec<_>>();
        let Some(selection) = disambiguate_exact_source_candidate(candidates.as_slice()) else {
            continue;
        };
        let module_match = module_package_match(
            &module_fingerprint,
            selection.source,
            ModuleMatchStrategy::NormalizedSourceHash,
            selection
                .source
                .function_signature_hashes
                .intersection(&module_fingerprint.function_signature_hashes)
                .count(),
            selection
                .source
                .string_anchors
                .intersection(&module_fingerprint.string_anchors)
                .count(),
            selection.external_importable,
        );
        if module_match.external_importable {
            report
                .attributions
                .push(accepted_attribution_from_match(&module_match));
            already_accepted.insert(module.id);
        }
        report
            .matches
            .push(PackageMatch::from_module_match(&module_match));
        matched_modules.insert(module.id);
    }
}
