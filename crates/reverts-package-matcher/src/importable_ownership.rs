//! Promote source-only ownership matches to external-import matches.
//!
//! When a module already matched a package by source (e.g. via cascade
//! coverage or normalised-source hash) and the corresponding package
//! source is itself externally importable, this pass upgrades the match
//! by attaching the import specifier and accepted-external attribution
//! so the emitter swaps the source for an ESM import.

use std::collections::BTreeMap;

use reverts_input::{InputRows, PackageAttributionInput};
use reverts_ir::{ModuleKind, split_bare_specifier};

use crate::{
    ExternalImportSourceIndex, PackageModuleSourceQuality, PackageSource,
    VersionedPackageMatchReport, accepted_external_modules, importable_package_source_for_module,
    package_module_source_quality, source_only_match_can_be_promoted_to_import,
};

pub(crate) fn promote_importable_ownership_matches(
    rows: &InputRows,
    package_sources: &[PackageSource],
    report: &mut VersionedPackageMatchReport,
) {
    let already_accepted = accepted_external_modules(rows, report);
    let modules_by_id = rows
        .modules
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let external_source_index = ExternalImportSourceIndex::build(package_sources);
    let mut promotions = Vec::<(usize, PackageAttributionInput, String, String)>::new();

    for (idx, package_match) in report.matches.iter().enumerate() {
        if package_match.external_importable || already_accepted.contains(&package_match.module_id)
        {
            continue;
        }
        if !source_only_match_can_be_promoted_to_import(package_match.strategy) {
            continue;
        }
        let Some(module) = modules_by_id.get(&package_match.module_id).copied() else {
            continue;
        };
        if module.kind != ModuleKind::Package
            || module.package_name.as_deref() != Some(package_match.package_name.as_str())
            || module.package_version.as_deref().is_some_and(|expected| {
                let expected = expected.trim();
                !expected.is_empty() && expected != package_match.package_version
            })
        {
            continue;
        }
        let Some(slice) = rows.module_source_slice(module.id) else {
            continue;
        };
        if package_module_source_quality(module, slice.source_file_path, slice.source)
            != PackageModuleSourceQuality::Trusted
        {
            continue;
        }
        let Some(import_target) = importable_package_source_for_module(
            module,
            package_match,
            &external_source_index,
            slice.source,
        ) else {
            continue;
        };
        let mut attribution = PackageAttributionInput::accepted_external(
            module.id,
            package_match.package_name.as_str(),
            package_match.package_version.as_str(),
            import_target.export_specifier.as_str(),
        )
        .with_resolved_file(import_target.source_path.as_str());
        if let Some((_package_name, Some(subpath))) =
            split_bare_specifier(import_target.export_specifier.as_str())
        {
            attribution = attribution.with_subpath(subpath);
        }
        promotions.push((
            idx,
            attribution,
            import_target.export_specifier,
            import_target.source_path,
        ));
    }

    for (idx, attribution, export_specifier, source_path) in promotions {
        if let Some(package_match) = report.matches.get_mut(idx) {
            package_match.external_importable = true;
            package_match.export_specifier = export_specifier;
            package_match.source_path = source_path;
        }
        report.attributions.push(attribution);
    }
}
