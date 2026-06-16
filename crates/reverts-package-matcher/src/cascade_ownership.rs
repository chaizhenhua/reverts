//! Promote per-function cascade match coverage into module-level
//! package ownership attributions.
//!
//! The cascade matcher produces per-function evidence
//! (`CascadeOwnershipMatch`). When the evidence for a module covers
//! every function in it (full coverage) or a strong majority (partial
//! coverage with high confidence), this pass admits the module as a
//! package-owned match, optionally promoting to an external import when
//! all contributing functions are exact-tier and share a single export
//! specifier.

use std::collections::{BTreeMap, BTreeSet};

use reverts_input::{
    InputRows, PackageAttributionInput, PackageAttributionStatus, PackageEmissionMode,
};
use reverts_ir::{FunctionFingerprint, MatchTier, ModuleId, ModuleKind, split_bare_specifier};

use crate::{
    CascadeMatchReport, CascadeOwnershipMatch, ModuleMatchStrategy, PackageMatch,
    VersionedPackageMatchReport,
};

pub(crate) fn promote_cascade_function_coverage_to_module_attributions(
    rows: &InputRows,
    fingerprints_by_module: &BTreeMap<ModuleId, Vec<FunctionFingerprint>>,
    cascade_report: &CascadeMatchReport,
    report: &mut VersionedPackageMatchReport,
) {
    let already_accepted = report
        .attributions
        .iter()
        .chain(rows.package_attributions.iter())
        .filter(|attribution| {
            attribution.status == PackageAttributionStatus::Accepted
                && attribution.emission_mode == PackageEmissionMode::ExternalImport
        })
        .map(|attribution| attribution.module_id)
        .collect::<BTreeSet<_>>();
    let matched_modules = report
        .matches
        .iter()
        .map(|package_match| package_match.module_id)
        .collect::<BTreeSet<_>>();
    let cascade_ownership_by_module = cascade_report.ownership_matches.iter().fold(
        BTreeMap::<ModuleId, Vec<&CascadeOwnershipMatch>>::new(),
        |mut by_module, ownership| {
            by_module
                .entry(ownership.module_id)
                .or_default()
                .push(ownership);
            by_module
        },
    );

    for module in &rows.modules {
        if module.kind != ModuleKind::Package
            || already_accepted.contains(&module.id)
            || matched_modules.contains(&module.id)
        {
            continue;
        }
        let Some(expected_package_name) = module.package_name.as_deref() else {
            continue;
        };
        let Some(fingerprints) = fingerprints_by_module.get(&module.id) else {
            continue;
        };
        let Some(cascade_ownership) = cascade_ownership_by_module.get(&module.id) else {
            continue;
        };
        if fingerprints.is_empty() {
            continue;
        }
        let mut ownership_by_package_version =
            BTreeMap::<(&str, &str), Vec<&CascadeOwnershipMatch>>::new();
        for ownership in cascade_ownership {
            ownership_by_package_version
                .entry((
                    ownership.package_name.as_str(),
                    ownership.package_version.as_str(),
                ))
                .or_default()
                .push(*ownership);
        }
        let mut ranked_ownership = ownership_by_package_version
            .into_iter()
            .map(|(package_version, ownership)| {
                let covered_spans = ownership
                    .iter()
                    .map(|ownership| ownership.function_span)
                    .collect::<BTreeSet<_>>();
                (package_version, ownership, covered_spans)
            })
            .collect::<Vec<_>>();
        ranked_ownership.sort_by(|left, right| {
            right
                .2
                .len()
                .cmp(&left.2.len())
                .then_with(|| left.0.cmp(&right.0))
        });
        let Some(((package_name, package_version), selected_ownership, covered_spans)) =
            ranked_ownership.first()
        else {
            continue;
        };
        let package_name = *package_name;
        let package_version = *package_version;
        if package_name != expected_package_name {
            continue;
        }
        if module
            .package_version
            .as_deref()
            .map(str::trim)
            .filter(|version| !version.is_empty())
            .is_some_and(|expected_version| package_version != expected_version)
        {
            continue;
        }

        let covered_count = covered_spans.len();
        let runner_up_count = ranked_ownership.get(1).map_or(0, |ranked| ranked.2.len());
        let is_full_coverage =
            covered_count == fingerprints.len() && cascade_ownership.len() == fingerprints.len();
        if !is_full_coverage
            && !accept_partial_cascade_coverage(
                fingerprints.len(),
                covered_count,
                cascade_ownership
                    .iter()
                    .map(|ownership| ownership.function_span)
                    .collect::<BTreeSet<_>>()
                    .len(),
                runner_up_count,
            )
        {
            continue;
        }

        let export_specifiers = selected_ownership
            .iter()
            .map(|ownership| ownership.export_specifier.as_str())
            .collect::<BTreeSet<_>>();
        let has_exact_function_import_proof = selected_ownership
            .iter()
            .all(cascade_ownership_has_exact_import_proof);
        let can_externalize = is_full_coverage
            && has_exact_function_import_proof
            && selected_ownership
                .iter()
                .all(|ownership| ownership.external_importable)
            && export_specifiers.len() == 1;
        let strategy = if is_full_coverage && has_exact_function_import_proof {
            ModuleMatchStrategy::CascadeFunctionCoverage
        } else if is_full_coverage {
            ModuleMatchStrategy::CascadeFunctionOwnership
        } else {
            ModuleMatchStrategy::CascadePartialFunctionCoverage
        };
        let export_specifier = export_specifiers.first().copied().unwrap_or(package_name);

        if can_externalize {
            let mut attribution = PackageAttributionInput::accepted_external(
                module.id,
                package_name,
                package_version,
                export_specifier,
            );
            if let Some((_package_name, Some(subpath))) = split_bare_specifier(export_specifier) {
                attribution = attribution.with_subpath(subpath);
            }
            report.attributions.push(attribution);
        }
        report.matches.push(PackageMatch {
            module_id: module.id,
            package_name: package_name.to_string(),
            package_version: package_version.to_string(),
            export_specifier: export_specifier.to_string(),
            source_path: format!("cascade:{export_specifier}"),
            normalized_source_hash: String::new(),
            strategy,
            function_signature_matches: covered_count,
            string_anchor_matches: 0,
            external_importable: can_externalize,
        });
    }
}

fn cascade_ownership_has_exact_import_proof(ownership: &&CascadeOwnershipMatch) -> bool {
    matches!(
        ownership.confidence.tier,
        MatchTier::Exact | MatchTier::ExactAlternate
    )
}

fn accept_partial_cascade_coverage(
    total_functions: usize,
    covered_functions: usize,
    ownership_spans: usize,
    runner_up_functions: usize,
) -> bool {
    if total_functions < 3 || covered_functions < 2 {
        return false;
    }
    if covered_functions * 100 < total_functions * 65 {
        return false;
    }
    if ownership_spans == 0 || covered_functions * 100 < ownership_spans * 80 {
        return false;
    }
    runner_up_functions == 0 || covered_functions >= runner_up_functions.saturating_mul(3)
}
