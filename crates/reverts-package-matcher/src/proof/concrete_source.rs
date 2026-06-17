use std::collections::{BTreeMap, BTreeSet};

use reverts_input::{InputRows, PackageAttributionStatus, PackageEmissionMode};
use reverts_ir::{ModuleId, ModuleKind, is_valid_package_name};
use reverts_package::external_import_concrete_source_path;

use crate::package_helpers::has_accepted_external_attribution;
use crate::{ConcretePackageSourcePath, VersionedPackageMatchReport};

pub(crate) fn unmatched_package_scope(rows: &InputRows) -> BTreeSet<String> {
    rows.modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
        .filter(|module| !has_accepted_external_attribution(rows, module.id))
        .filter_map(|module| {
            module
                .package_name
                .as_deref()
                .map(str::trim)
                .filter(|package_name| {
                    !package_name.is_empty() && is_valid_package_name(package_name)
                })
                .map(ToOwned::to_owned)
        })
        .collect()
}

pub(crate) fn concrete_package_sources_by_module(
    rows: &InputRows,
    report: &VersionedPackageMatchReport,
) -> BTreeMap<ModuleId, ConcretePackageSourcePath> {
    let mut sources = BTreeMap::new();
    for attribution in rows
        .package_attributions
        .iter()
        .chain(report.attributions.iter())
    {
        if attribution.status != PackageAttributionStatus::Accepted
            || attribution.emission_mode != PackageEmissionMode::ExternalImport
        {
            continue;
        }
        let Some(package_version) = attribution.package_version.as_deref() else {
            continue;
        };
        let Some(resolved_file) = attribution.resolved_file.as_deref() else {
            continue;
        };
        if let Some(concrete) = concrete_package_source_from_parts(
            attribution.module_id,
            attribution.package_name.as_str(),
            package_version,
            resolved_file,
        ) {
            sources.insert(attribution.module_id, concrete);
        }
    }
    for package_match in &report.matches {
        if let Some(concrete) = concrete_package_source_from_parts(
            package_match.module_id,
            package_match.package_name.as_str(),
            package_match.package_version.as_str(),
            package_match.source_path.as_str(),
        ) {
            sources.entry(package_match.module_id).or_insert(concrete);
        }
    }
    sources
}

pub(crate) fn concrete_package_source_from_parts(
    _module_id: ModuleId,
    package_name: &str,
    package_version: &str,
    proof_path: &str,
) -> Option<ConcretePackageSourcePath> {
    let source_path = concrete_package_source_path_from_proof(proof_path)?;
    Some(ConcretePackageSourcePath {
        package_name: package_name.to_string(),
        package_version: package_version.to_string(),
        source_path,
    })
}

fn concrete_package_source_path_from_proof(proof_path: &str) -> Option<String> {
    let proof_path = proof_path.trim();
    if proof_path.is_empty()
        || proof_path.starts_with("exact-hint:")
        || proof_path.starts_with("dependency-closure:")
        || proof_path.starts_with("dependency-cluster:")
        || proof_path.starts_with("package-file-graph:")
        || proof_path.starts_with("aggregate:")
        || proof_path.starts_with("cascade:")
        || proof_path.starts_with("structural-bag:")
    {
        return None;
    }
    if let Some(source_path) = external_import_concrete_source_path(proof_path) {
        return Some(source_path);
    }
    Some(proof_path.to_string())
}

pub(crate) fn package_version_from_proof_path(
    package_name: &str,
    proof_path: &str,
) -> Option<String> {
    let concrete = concrete_package_source_path_from_proof(proof_path)?;
    let prefix = format!("{package_name}@");
    let rest = concrete.strip_prefix(prefix.as_str())?;
    let (version, _path) = rest.split_once('/')?;
    (!version.trim().is_empty()).then(|| version.to_string())
}
