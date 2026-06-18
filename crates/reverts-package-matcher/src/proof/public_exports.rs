use std::collections::{BTreeMap, BTreeSet};

use crate::package_helpers::package_source_external_import_rank;
use crate::source::exported_members::{export_member_set_is_strong, is_usable_export_member};
use crate::source::package_refs::{
    PackageReexportEdgeKind, package_source_reexported_source_only_sources,
};
use crate::{ExternalImportSourceIndex, PackagePublicExportProof, PackageSource};

#[must_use]
pub fn package_source_public_export_proofs(
    package_sources: &[PackageSource],
) -> Vec<PackagePublicExportProof> {
    let external_source_index = ExternalImportSourceIndex::build(package_sources);
    let mut candidates_by_source_path =
        BTreeMap::<String, Vec<(&PackageSource, BTreeSet<String>)>>::new();

    for external in package_sources
        .iter()
        .filter(|source| source.external_importable)
    {
        for source in package_source_reexported_source_only_sources(
            external,
            &external_source_index,
            PackageReexportEdgeKind::AnyReexport,
        ) {
            let public_members = external_source_index
                .export_members(source)
                .into_iter()
                .filter(|member| is_usable_export_member(member))
                .collect::<BTreeSet<_>>();
            if !export_member_set_is_strong(public_members.iter()) {
                continue;
            }
            candidates_by_source_path
                .entry(source.source_path.clone())
                .or_default()
                .push((external, public_members));
        }
    }

    let mut proofs = Vec::new();
    for (source_path, mut candidates) in candidates_by_source_path {
        candidates.sort_by(|left, right| {
            package_source_external_import_rank(left.0)
                .cmp(&package_source_external_import_rank(right.0))
                .then_with(|| left.0.export_specifier.cmp(&right.0.export_specifier))
                .then_with(|| left.0.source_path.cmp(&right.0.source_path))
        });
        let Some((best_external, _)) = candidates.first() else {
            continue;
        };
        let best_rank = package_source_external_import_rank(best_external);
        let best = candidates
            .into_iter()
            .filter(|(external, _)| package_source_external_import_rank(external) == best_rank)
            .collect::<Vec<_>>();
        let export_specifiers = best
            .iter()
            .map(|(external, _)| external.export_specifier.as_str())
            .collect::<BTreeSet<_>>();
        if export_specifiers.len() != 1 {
            continue;
        }
        let export_specifier = export_specifiers
            .into_iter()
            .next()
            .expect("one export specifier")
            .to_string();
        let Some((source, public_members)) =
            best.into_iter().next().and_then(|(external, members)| {
                external_source_index
                    .all_sources_for_package(external.package_name.as_str())
                    .into_iter()
                    .find(|candidate| candidate.source_path == source_path)
                    .map(|source| (source, members))
            })
        else {
            continue;
        };
        proofs.push(PackagePublicExportProof {
            package_name: source.package_name.clone(),
            package_version: source.package_version.clone(),
            source_path,
            export_specifier,
            public_members,
        });
    }

    proofs.sort_by(|left, right| {
        left.package_name
            .cmp(&right.package_name)
            .then_with(|| left.package_version.cmp(&right.package_version))
            .then_with(|| left.source_path.cmp(&right.source_path))
            .then_with(|| left.export_specifier.cmp(&right.export_specifier))
    });
    proofs
}
