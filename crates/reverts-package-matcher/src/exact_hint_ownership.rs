//! Exact-hint package ownership promotion.
//!
//! Modules carrying a strong upstream `(package_name, package_version)`
//! hint that matches a known package source are promoted to accepted
//! matches here, optionally with an external import specifier when the
//! upstream source is importable from the bundle's entry contract.

use std::collections::BTreeSet;

use reverts_input::{InputRows, PackageAttributionInput};
use reverts_ir::{ModuleId, ModuleKind, split_bare_specifier};
use semver::Version;

use crate::{
    ModuleMatchStrategy, PackageMatch, PackageModuleSourceQuality, PackageSource,
    VersionedPackageMatchReport, accepted_external_modules, is_build_path_segment,
    is_json_source_path, package_module_source_quality, package_module_source_quality_label,
    package_semantic_path_prefixes, strip_source_extension,
};

pub(crate) fn promote_exact_hint_ownership_matches(
    rows: &InputRows,
    package_sources: &[PackageSource],
    report: &mut VersionedPackageMatchReport,
) {
    let available_versions = package_sources
        .iter()
        .map(|source| {
            (
                source.package_name.as_str().to_string(),
                source.package_version.as_str().to_string(),
            )
        })
        .collect::<BTreeSet<_>>();
    if available_versions.is_empty() {
        return;
    }

    let mut already_accepted = accepted_external_modules(rows, report);
    let mut matched_modules = report
        .matches
        .iter()
        .map(|package_match| package_match.module_id)
        .collect::<BTreeSet<_>>();

    for module in &rows.modules {
        if module.kind != ModuleKind::Package || already_accepted.contains(&module.id) {
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
            .filter(|package_version| !package_version.is_empty())
            .filter(|package_version| Version::parse(package_version).is_ok())
        else {
            continue;
        };
        if !available_versions.contains(&(package_name.to_string(), package_version.to_string())) {
            continue;
        }
        let Some(slice) = rows.module_source_slice(module.id) else {
            continue;
        };
        let quality = package_module_source_quality(module, slice.source_file_path, slice.source);
        if quality == PackageModuleSourceQuality::Invalid {
            continue;
        }
        let external_specifier = (quality == PackageModuleSourceQuality::Trusted)
            .then(|| {
                exact_hint_external_specifier(
                    package_sources,
                    package_name,
                    package_version,
                    module.semantic_path.as_str(),
                )
            })
            .flatten();
        let external_importable = external_specifier.is_some();
        let export_specifier = external_specifier.unwrap_or_else(|| package_name.to_string());
        if matched_modules.contains(&module.id) {
            if external_importable
                && let Some(existing_match) = report.matches.iter_mut().find(|package_match| {
                    package_match.module_id == module.id
                        && package_match.package_name == package_name
                        && package_match.package_version == package_version
                })
            {
                existing_match.export_specifier = export_specifier.clone();
                existing_match.source_path = exact_hint_source_path(
                    package_name,
                    package_version,
                    quality,
                    module.semantic_path.as_str(),
                );
                existing_match.external_importable = true;
                report
                    .attributions
                    .push(accepted_external_attribution_for_module(
                        module.id,
                        package_name,
                        package_version,
                        export_specifier.as_str(),
                    ));
                already_accepted.insert(module.id);
            }
            continue;
        }
        matched_modules.insert(module.id);
        if external_importable {
            report
                .attributions
                .push(accepted_external_attribution_for_module(
                    module.id,
                    package_name,
                    package_version,
                    export_specifier.as_str(),
                ));
            already_accepted.insert(module.id);
        }
        report.matches.push(PackageMatch {
            module_id: module.id,
            package_name: package_name.to_string(),
            package_version: package_version.to_string(),
            export_specifier,
            source_path: exact_hint_source_path(
                package_name,
                package_version,
                quality,
                module.semantic_path.as_str(),
            ),
            normalized_source_hash: String::new(),
            strategy: ModuleMatchStrategy::DependencyClosureOwnership,
            function_signature_matches: 0,
            string_anchor_matches: 0,
            external_importable,
        });
    }
}

fn accepted_external_attribution_for_module(
    module_id: ModuleId,
    package_name: &str,
    package_version: &str,
    export_specifier: &str,
) -> PackageAttributionInput {
    let mut attribution = PackageAttributionInput::accepted_external(
        module_id,
        package_name,
        package_version,
        export_specifier,
    );
    if let Some((_package_name, Some(subpath))) = split_bare_specifier(export_specifier) {
        attribution = attribution.with_subpath(subpath);
    }
    attribution
}

fn exact_hint_source_path(
    package_name: &str,
    package_version: &str,
    quality: PackageModuleSourceQuality,
    semantic_path: &str,
) -> String {
    format!(
        "exact-hint:{package_name}@{package_version}:quality={}:semantic_path={semantic_path}",
        package_module_source_quality_label(quality),
    )
}

fn exact_hint_external_specifier(
    package_sources: &[PackageSource],
    package_name: &str,
    package_version: &str,
    semantic_path: &str,
) -> Option<String> {
    let specifiers = package_sources
        .iter()
        .filter(|source| {
            source.package_name == package_name
                && source.package_version == package_version
                && source.external_importable
                && !is_json_source_path(source.source_path.as_str())
                && source.export_specifier == package_name
                && (semantic_path_is_package_root(package_name, semantic_path)
                    || semantic_path_matches_package_source_entry(semantic_path, source))
        })
        .map(|source| source.export_specifier.clone())
        .collect::<BTreeSet<_>>();
    (specifiers.len() == 1).then(|| {
        specifiers
            .into_iter()
            .next()
            .expect("one root external specifier")
    })
}

fn semantic_path_matches_package_source_entry(semantic_path: &str, source: &PackageSource) -> bool {
    normalized_semantic_path(semantic_path)
        .is_some_and(|semantic_path| semantic_path == package_source_semantic_entry_path(source))
}

fn normalized_semantic_path(path: &str) -> Option<String> {
    let clean = path
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/')
        .replace('\\', "/");
    let clean = strip_source_extension(clean.as_str()).trim_matches('/');
    let clean = strip_generated_module_semantic_prefix(clean);
    (!clean.is_empty()).then(|| clean.to_string())
}

fn strip_generated_module_semantic_prefix(path: &str) -> &str {
    let Some(rest) = path.strip_prefix("modules/") else {
        return path;
    };
    let Some((id, semantic_path)) = rest.split_once('-') else {
        return path;
    };
    if !id.is_empty() && id.chars().all(|ch| ch.is_ascii_digit()) && !semantic_path.is_empty() {
        semantic_path
    } else {
        path
    }
}

fn package_source_semantic_entry_path(source: &PackageSource) -> String {
    let prefix = format!("{}@{}/", source.package_name, source.package_version);
    let entry_path = source
        .source_path
        .strip_prefix(prefix.as_str())
        .unwrap_or(source.source_path.as_str());
    normalized_semantic_path(entry_path).unwrap_or_default()
}

fn semantic_path_is_package_root(package_name: &str, semantic_path: &str) -> bool {
    let Some(clean) = normalized_semantic_path(semantic_path) else {
        return false;
    };
    for prefix in package_semantic_path_prefixes(package_name) {
        let prefix = prefix.trim_matches('/');
        if clean.as_str() == prefix {
            return true;
        }
        if let Some(rest) = clean.as_str().strip_prefix(format!("{prefix}/").as_str()) {
            let rest = strip_source_extension(rest).trim_matches('/');
            if rest.is_empty()
                || rest == "index"
                || rest
                    .split('/')
                    .all(|segment| is_build_path_segment(segment) || segment == "index")
            {
                return true;
            }
        }
    }
    false
}
