//! Exact-hint package ownership promotion.
//!
//! Modules carrying a strong upstream `(package_name, package_version)`
//! hint that matches a known package source are promoted to accepted
//! matches here, optionally with an external import specifier when the
//! upstream source is importable from the bundle's entry contract.

use std::collections::{BTreeMap, BTreeSet};

use reverts_input::{InputRows, ModuleInput, PackageAttributionInput};
use reverts_ir::{ModuleId, ModuleKind, split_bare_specifier};
use reverts_package::ExternalImportProofPath;
use semver::Version;

use crate::{
    ModuleMatchStrategy, PackageMatch, PackageModuleSourceQuality, PackageSource,
    VersionedPackageMatchReport, accepted_external_modules, is_build_path_segment,
    is_json_source_path, module_match_fingerprint, package_module_source_quality,
    package_module_source_quality_label, package_semantic_path_prefixes,
    package_source_fingerprint, strip_source_extension,
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
    // Normalized source hash -> the package names that ship a source with that
    // hash. Used to fingerprint-verify a weak path hint: if a module's source
    // positively matches some other package, the hint is mis-attributed and
    // must not be trusted (e.g. ws Receiver code carrying a `node_modules/zod`
    // path hint from the bundle).
    let package_names_by_source_hash = package_names_by_source_hash(package_sources);

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
        // A path hint can be mis-attributed by the bundle: e.g. ws Receiver code
        // shipped under a `node_modules/zod` path hint. When the module's
        // normalized source positively matches a DIFFERENT package's shipped
        // source, the hint provably names the wrong package. Keep the source-only
        // ownership match so the cross-package correction in `force_externalize`
        // can re-home the module to its true package, but never externalize it to
        // the contradicted package from here — including the false-`Trusted` case
        // where a short package name leaves no in-source token to verify.
        let contradicted = hint_contradicted_by_source_fingerprint(
            module,
            slice.source_file_path,
            slice.source,
            package_name,
            &package_names_by_source_hash,
        );
        let external_specifier = (quality == PackageModuleSourceQuality::Trusted && !contradicted)
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
    ExternalImportProofPath::exact_hint(
        package_name,
        package_version,
        package_module_source_quality_label(quality),
        semantic_path,
    )
}

/// Index every cached package source by its normalized source hashes, mapping
/// each hash to the set of package names that ship a source with that hash.
fn package_names_by_source_hash(
    package_sources: &[PackageSource],
) -> BTreeMap<String, BTreeSet<String>> {
    // The per-source parse + normalize dominates this pass; fan it out across
    // cores, then fold into the map serially in input order (deterministic).
    let per_source: Vec<(String, Vec<String>)> = crate::par_map(package_sources, |source| {
        match package_source_fingerprint(source) {
            Ok(fingerprint) => (
                source.package_name.clone(),
                fingerprint
                    .normalized_source_hashes
                    .iter()
                    .cloned()
                    .collect::<Vec<String>>(),
            ),
            Err(_) => (source.package_name.clone(), Vec::<String>::new()),
        }
    });
    let mut by_hash = BTreeMap::<String, BTreeSet<String>>::new();
    for (package_name, hashes) in per_source {
        for hash in hashes {
            by_hash
                .entry(hash)
                .or_default()
                .insert(package_name.clone());
        }
    }
    by_hash
}

/// Whether a path hint claiming `package_name` is contradicted by source
/// fingerprint evidence. Returns true only when the module's normalized source
/// positively matches at least one cached package source AND none of the
/// matching packages is the claimed one — i.e. the source provably belongs to a
/// different package. A module that matches nothing (common for minified
/// surfaces) is not contradicted.
fn hint_contradicted_by_source_fingerprint(
    module: &ModuleInput,
    source_path: &str,
    source: &str,
    package_name: &str,
    package_names_by_source_hash: &BTreeMap<String, BTreeSet<String>>,
) -> bool {
    let Ok(fingerprint) = module_match_fingerprint(module, source_path, source) else {
        return false;
    };
    let mut matched_packages = BTreeSet::<&str>::new();
    for hash in &fingerprint.normalized_source_hashes {
        if let Some(packages) = package_names_by_source_hash.get(hash) {
            matched_packages.extend(packages.iter().map(String::as_str));
        }
    }
    !matched_packages.is_empty() && !matched_packages.contains(package_name)
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
