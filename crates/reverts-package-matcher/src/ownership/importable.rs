//! Promote source-only ownership matches to external-import matches.
//!
//! When a module already matched a package by source (e.g. via cascade
//! coverage or normalised-source hash) and the corresponding package
//! source is itself externally importable, this pass upgrades the match
//! by attaching the import specifier and accepted-external attribution
//! so the emitter swaps the source for an ESM import.

use std::collections::BTreeMap;

use reverts_input::InputRows;
use reverts_ir::ModuleKind;

use crate::pipeline::is_anonymous_bundle_package_candidate;
use crate::{
    ExternalImportSourceIndex, PackageModuleSourceQuality, PackageSource,
    VersionedPackageMatchReport, accepted_external_modules, importable_package_source_for_module,
    package_module_source_quality, public_export_member_external_package_source,
    source_only_match_can_be_promoted_to_import,
};

use super::promotion::{ExternalImportPromotion, apply_external_import_promotion};

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
    let mut promotions = Vec::<(usize, ExternalImportPromotion)>::new();

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
        promotions.push((
            idx,
            ExternalImportPromotion {
                module_id: module.id,
                package_name: package_match.package_name.clone(),
                package_version: package_match.package_version.clone(),
                export_specifier: import_target.export_specifier,
                resolved_file: import_target.source_path,
                strategy: package_match.strategy,
                function_signature_matches: package_match.function_signature_matches,
                string_anchor_matches: package_match.string_anchor_matches,
            },
        ));
    }

    for (idx, promotion) in promotions {
        apply_external_import_promotion(report, Some(idx), promotion);
    }
}

/// Promote *anonymous bundle* ownership matches to external imports when the
/// inlined module's public surface is provably a subset of the recognized
/// package's public exports.
///
/// esbuild inlines third-party packages as anonymous `Application` modules
/// (no `package_name`, minified internal binding names) cross-referenced by
/// internal bindings. Recognition passes label such a module with its true
/// package (`PackageMatch.package_name`) but deliberately leave it
/// `external_importable = false`, because naively swapping the body for an
/// `import { … } from "pkg"` would break every consumer that reads one of the
/// module's *internal* bindings (absent from the package's public surface).
///
/// This pass lifts that block ONLY through the public-export-member proof
/// (`public_export_member_external_package_source`), which requires the
/// module's own exported members to be a subset of the package's public
/// exports (and resolves to a single import specifier with a recovered
/// minified-local → public-export alias map). The planner's external-package
/// adapter is the final airtight gate: if any *consumed* binding cannot be
/// re-provided from the package's public surface, the planner keeps the
/// module's real source inlined instead, so the build never breaks.
pub(crate) fn promote_anonymous_bundle_external_imports(
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
    let mut promotions = Vec::<(usize, ExternalImportPromotion)>::new();

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
        if !is_anonymous_bundle_package_candidate(rows, module) {
            continue;
        }
        let Some(slice) = rows.module_source_slice(module.id) else {
            continue;
        };
        let Some(import_target) = public_export_member_external_package_source(
            module,
            package_match,
            &external_source_index,
            slice.source,
        ) else {
            continue;
        };
        promotions.push((
            idx,
            ExternalImportPromotion {
                module_id: module.id,
                package_name: package_match.package_name.clone(),
                package_version: package_match.package_version.clone(),
                export_specifier: import_target.export_specifier,
                resolved_file: import_target.source_path,
                strategy: package_match.strategy,
                function_signature_matches: package_match.function_signature_matches,
                string_anchor_matches: package_match.string_anchor_matches,
            },
        ));
    }

    for (idx, promotion) in promotions {
        apply_external_import_promotion(report, Some(idx), promotion);
    }
}
