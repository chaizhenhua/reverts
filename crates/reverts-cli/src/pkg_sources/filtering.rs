//! Prune redundant or irrelevant package sources before the matcher
//! consumes them. Sources are bucketed by build-variant family (e.g.
//! `dist/esm` vs `dist/cjs`) and the family whose surface best matches
//! the project's exact-version path hints is kept. A second pass drops
//! sources whose body has nothing in common with any hint, and a final
//! dedup step keeps the externally-importable copy of each
//! `(package, version, path)` triple.

use std::collections::{BTreeMap, BTreeSet};

use reverts_input::InputRows;
use reverts_ir::ModuleKind;
use reverts_package_matcher::{
    PackageSource, clean_package_semantic_path_hint, is_exact_package_version_hint,
    normalize_hint_text, package_source_semantic_surface_hint_score,
};

use crate::persistence::source_cache::package_source_cache_entry_path;

pub(crate) fn filter_package_sources_to_best_build_variants(
    rows: &InputRows,
    package_sources: &mut Vec<PackageSource>,
) {
    let hints_by_version = package_path_hints_by_version(rows);
    if hints_by_version.is_empty() {
        return;
    }

    let mut source_families_by_version =
        BTreeMap::<(String, String), BTreeMap<String, Vec<usize>>>::new();
    for (index, source) in package_sources.iter().enumerate() {
        let key = (source.package_name.clone(), source.package_version.clone());
        let rel_path = package_source_cache_entry_path(source);
        source_families_by_version
            .entry(key)
            .or_default()
            .entry(build_variant_family_key(rel_path.as_str()))
            .or_default()
            .push(index);
    }

    let mut selected_families_by_version = BTreeMap::<(String, String), BTreeSet<String>>::new();
    for (key, family_paths) in source_families_by_version {
        let Some(hints) = hints_by_version.get(&key) else {
            continue;
        };
        let mut scored = family_paths
            .into_iter()
            .map(|(family, paths)| {
                let matched_hints = hints
                    .iter()
                    .map(|hint| {
                        paths
                            .iter()
                            .map(|index| {
                                package_source_semantic_filter_hint_score(
                                    &package_sources[*index],
                                    hint,
                                )
                            })
                            .max()
                            .unwrap_or(0)
                    })
                    .sum::<usize>();
                let has_external_importable = paths
                    .iter()
                    .any(|index| package_sources[*index].external_importable);
                (family, matched_hints, has_external_importable)
            })
            .filter(|(_family, matched_hints, _has_external_importable)| *matched_hints > 0)
            .collect::<Vec<_>>();
        if scored.is_empty() {
            continue;
        }
        scored.sort_by(|left, right| {
            right
                .1
                .cmp(&left.1)
                .then_with(|| {
                    build_variant_family_rank(left.0.as_str())
                        .cmp(&build_variant_family_rank(right.0.as_str()))
                })
                .then_with(|| left.0.cmp(&right.0))
        });
        let best_score = scored[0].1;
        let best_rank = build_variant_family_rank(scored[0].0.as_str());
        let best_external_rank = scored
            .iter()
            .filter(|(_family, score, has_external_importable)| {
                *score == best_score && *has_external_importable
            })
            .map(|(family, _score, _has_external_importable)| {
                build_variant_family_rank(family.as_str())
            })
            .min();
        let selected = scored
            .into_iter()
            .filter(|(family, score, has_external_importable)| {
                if *score != best_score {
                    return false;
                }
                let rank = build_variant_family_rank(family.as_str());
                if best_score < 2 || rank == best_rank {
                    return true;
                }
                best_external_rank
                    .is_some_and(|external_rank| *has_external_importable && rank == external_rank)
            })
            .map(|(family, _score, _has_external_importable)| family)
            .collect::<BTreeSet<_>>();
        selected_families_by_version.insert(key, selected);
    }

    if selected_families_by_version.is_empty() {
        return;
    }

    package_sources.retain(|source| {
        if is_root_package_manifest(source)
            || (source.external_importable && source.export_specifier == source.package_name)
        {
            return true;
        }
        let key = (source.package_name.clone(), source.package_version.clone());
        let Some(selected_families) = selected_families_by_version.get(&key) else {
            return true;
        };
        let rel_path = package_source_cache_entry_path(source);
        selected_families.contains(build_variant_family_key(rel_path.as_str()).as_str())
    });
}

const PACKAGE_SOURCE_PATH_HINT_FILTER_MIN_SOURCES: usize = 256;

pub(crate) fn filter_package_sources_to_relevant_path_hints(
    rows: &InputRows,
    package_sources: &mut Vec<PackageSource>,
) {
    let hints_by_version = package_path_hints_by_version(rows);
    if hints_by_version.is_empty() {
        return;
    }
    let counts_by_version = package_sources.iter().fold(
        BTreeMap::<(String, String), usize>::new(),
        |mut counts, source| {
            *counts
                .entry((source.package_name.clone(), source.package_version.clone()))
                .or_default() += 1;
            counts
        },
    );
    package_sources.retain(|source| {
        let key = (source.package_name.clone(), source.package_version.clone());
        if counts_by_version.get(&key).copied().unwrap_or_default()
            <= PACKAGE_SOURCE_PATH_HINT_FILTER_MIN_SOURCES
        {
            return true;
        }
        let Some(hints) = hints_by_version.get(&key) else {
            return true;
        };
        if is_root_package_manifest(source)
            || (source.external_importable && source.export_specifier == source.package_name)
        {
            return true;
        }
        hints
            .iter()
            .any(|hint| package_source_semantic_filter_hint_score(source, hint.as_str()) > 0)
    });
}

/// Whether `source` is a package's root `package.json` manifest. The
/// cache-anchored surface resolver reads it to derive each package's real
/// public export specifiers, so it must survive every build-variant / path-hint
/// prune even when its own `export_specifier` is a subpath (e.g. rxjs exposes
/// `./package.json`, giving the manifest the specifier `rxjs/package.json`).
fn is_root_package_manifest(source: &PackageSource) -> bool {
    package_source_cache_entry_path(source) == "package.json"
}

fn package_source_semantic_filter_hint_score(source: &PackageSource, hint: &str) -> usize {
    package_source_semantic_surface_hint_score(source, hint).max(
        package_source_body_semantic_hint_score(source.source.as_str(), hint),
    )
}

fn package_source_body_semantic_hint_score(source: &str, hint: &str) -> usize {
    let hint = hint.trim().trim_matches('/');
    if hint.is_empty() {
        return 0;
    }
    let hint_last_segment = hint.rsplit('/').next().unwrap_or(hint);
    let hint_last_normalized = normalize_hint_text(hint_last_segment);
    if hint_last_normalized.len() < 4 {
        return 0;
    }
    let source_normalized = normalize_hint_text(source);
    if source_normalized.contains(hint_last_normalized.as_str()) {
        2
    } else {
        0
    }
}

fn package_path_hints_by_version(rows: &InputRows) -> BTreeMap<(String, String), BTreeSet<String>> {
    let mut hints = BTreeMap::<(String, String), BTreeSet<String>>::new();
    for module in &rows.modules {
        if module.kind != ModuleKind::Package {
            continue;
        }
        let Some(package_name) = module.package_name.as_deref().map(str::trim) else {
            continue;
        };
        let Some(package_version) = module
            .package_version
            .as_deref()
            .map(str::trim)
            .filter(|version| is_exact_package_version_hint(version))
        else {
            continue;
        };
        let Some(hint) =
            clean_package_semantic_path_hint(package_name, module.semantic_path.as_str())
        else {
            continue;
        };
        hints
            .entry((package_name.to_string(), package_version.to_string()))
            .or_default()
            .insert(hint);
    }
    hints
}

fn build_variant_family_key(rel_path: &str) -> String {
    let lower = rel_path.to_ascii_lowercase();
    let parts = lower.split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        ["dist", second, ..]
            if matches!(
                *second,
                "cjs"
                    | "commonjs"
                    | "esm"
                    | "es"
                    | "es5"
                    | "es2015"
                    | "es2020"
                    | "module"
                    | "browser"
                    | "node"
                    | "umd"
                    | "bundles"
                    | "fesm5"
                    | "fesm2015"
                    | "fesm2020"
                    | "fesm2022"
            ) =>
        {
            format!("dist/{second}")
        }
        ["lib", second, ..]
            if matches!(
                *second,
                "cjs"
                    | "commonjs"
                    | "esm"
                    | "es"
                    | "es5"
                    | "es2015"
                    | "es2020"
                    | "module"
                    | "browser"
                    | "node"
                    | "umd"
            ) =>
        {
            format!("lib/{second}")
        }
        [first, ..]
            if matches!(
                *first,
                "dist"
                    | "lib"
                    | "cjs"
                    | "commonjs"
                    | "esm"
                    | "es"
                    | "es5"
                    | "es2015"
                    | "es2020"
                    | "module"
                    | "umd"
                    | "bundles"
                    | "fesm5"
                    | "fesm2015"
                    | "fesm2020"
                    | "fesm2022"
                    | "build"
                    | "src"
            ) =>
        {
            (*first).to_string()
        }
        _ => "root".to_string(),
    }
}

fn build_variant_family_rank(family: &str) -> u8 {
    match family {
        "dist/esm" | "dist/es" | "dist/es5" | "dist/es2015" | "dist/es2020" | "dist/module"
        | "lib/esm" | "lib/es" | "lib/es5" | "lib/es2015" | "lib/es2020" | "lib/module" | "esm"
        | "es" | "es5" | "es2015" | "es2020" | "module" => 0,
        "dist/cjs" | "dist/commonjs" | "lib/cjs" | "lib/commonjs" | "cjs" | "commonjs" => 1,
        "dist/node" | "lib/node" => 2,
        "dist/umd" | "dist/bundles" | "lib/umd" | "umd" | "bundles" => 3,
        "dist/fesm5" | "dist/fesm2015" | "dist/fesm2020" | "dist/fesm2022" | "fesm5"
        | "fesm2015" | "fesm2020" | "fesm2022" => 4,
        "dist" | "lib" | "build" => 5,
        "root" => 6,
        "src" => 7,
        _ => 8,
    }
}

pub(crate) fn dedup_package_sources(package_sources: &mut Vec<PackageSource>) {
    package_sources.sort_by(|left, right| {
        (
            left.package_name.as_str(),
            left.package_version.as_str(),
            left.source_path.as_str(),
        )
            .cmp(&(
                right.package_name.as_str(),
                right.package_version.as_str(),
                right.source_path.as_str(),
            ))
            .then_with(|| right.external_importable.cmp(&left.external_importable))
    });
    let mut seen = BTreeSet::new();
    package_sources.retain(|source| {
        seen.insert((
            source.package_name.clone(),
            source.package_version.clone(),
            source.source_path.clone(),
        ))
    });
}
