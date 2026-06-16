//! Runtime-dependency collection + scope-coherent version selection.
//!
//! Given an `InputBundle` already populated with package attributions
//! and surfaces, this module produces the `RuntimeDependency` list that
//! the generated project's `package.json` should pin. The main subtlety
//! is `pick_scope_coherent_runtime_dependency_versions`: when several
//! packages in the same `@scope/*` namespace got attributed independently,
//! we have to align their majors or the produced lockfile mixes
//! incompatible versions of sibling packages (see
//! docs/research/project-1-runtime-verification.md).

use std::collections::{BTreeMap, BTreeSet};

use reverts_input::{InputBundle, PackageAttributionStatus};
use semver::Version;

use crate::RuntimeDependency;

pub(crate) fn collect_runtime_dependencies(input: &InputBundle) -> Vec<RuntimeDependency> {
    // First, collect ALL candidate (package, version) pairs that any
    // attribution or surface row referenced. We need the full set so the
    // scope-coherence pass can downgrade to a major shared across the
    // namespace, not just the newest each package ever saw.
    let mut candidates: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for attribution in &input.package_attributions {
        if attribution.status != PackageAttributionStatus::Accepted
            || !attribution.emission_mode.requires_runtime_dependency()
        {
            continue;
        }
        let Some(package_version) = attribution.package_version.as_deref() else {
            continue;
        };
        candidates
            .entry(attribution.package_name.clone())
            .or_default()
            .insert(package_version.to_string());
    }
    for package_surface in &input.package_surfaces {
        if package_surface.status != PackageAttributionStatus::Accepted {
            continue;
        }
        let Some(package_version) = package_surface.package_version.as_deref() else {
            continue;
        };
        candidates
            .entry(package_surface.package_name.clone())
            .or_default()
            .insert(package_version.to_string());
    }

    let chosen = pick_scope_coherent_runtime_dependency_versions(&candidates);
    chosen
        .into_iter()
        .map(|(package_name, package_version)| RuntimeDependency {
            package_name,
            package_version,
        })
        .collect()
}

/// Pick one version per package, preferring versions whose major aligns
/// across packages in the same `@scope/*` namespace.
///
/// Why: the matcher attributes modules to packages and versions
/// independently. For sibling packages in a single namespace (the most
/// common offender: `@sentry/*` mixing v7 and v8) picking the highest
/// version per package can produce a `package.json` that ships
/// `@sentry/core@8.x` alongside `@sentry/node@7.x`. At runtime the v7
/// node integration calls into v8 core internals that no longer exist, so
/// startup crashes inside an `_optionalChain` polyfill before the CLI
/// even parses argv. See `docs/research/project-1-runtime-verification.md`
/// for the captured trace.
///
/// Strategy:
///   1. Group packages by `@scope` prefix (an empty group otherwise).
///   2. Within a scope, score each candidate major by how many module
///      attributions across the scope reference it (using set
///      membership as a proxy — every distinct version a package was
///      attributed to counts once).
///   3. Pick the winning major: the one referenced by the most packages
///      in the scope. Ties go to the higher major.
///   4. For every package in the scope, choose the highest version
///      whose major matches the winning major if available; otherwise
///      fall back to that package's own highest version (the package
///      is single-major and must stay there — flagging it would be
///      noisy and the manual pin documented in research notes is the
///      operator-facing workaround).
fn pick_scope_coherent_runtime_dependency_versions(
    candidates: &BTreeMap<String, BTreeSet<String>>,
) -> BTreeMap<String, String> {
    let mut by_scope: BTreeMap<Option<String>, Vec<&String>> = BTreeMap::new();
    for package_name in candidates.keys() {
        by_scope
            .entry(scope_prefix(package_name))
            .or_default()
            .push(package_name);
    }

    let mut chosen: BTreeMap<String, String> = BTreeMap::new();
    for (_scope, packages) in by_scope {
        let target_major = scope_target_major(&packages, candidates);
        for package_name in packages {
            let versions = &candidates[package_name];
            let pick = preferred_version_in_major(versions, target_major)
                .or_else(|| highest_version(versions));
            if let Some(v) = pick {
                chosen.insert(package_name.clone(), v);
            }
        }
    }
    chosen
}

fn scope_prefix(package_name: &str) -> Option<String> {
    if let Some(rest) = package_name.strip_prefix('@')
        && let Some(slash) = rest.find('/')
    {
        return Some(format!("@{}", &rest[..slash]));
    }
    None
}

fn scope_target_major(
    packages: &[&String],
    candidates: &BTreeMap<String, BTreeSet<String>>,
) -> Option<u64> {
    // Only attempt coherence when more than one scoped package is present;
    // a single package has nothing to align with so its own highest
    // version (handled by the caller's fallback) is correct.
    if packages.len() < 2 {
        return None;
    }
    let mut major_packages: BTreeMap<u64, BTreeSet<String>> = BTreeMap::new();
    for package_name in packages {
        for version_str in &candidates[*package_name] {
            if let Ok(version) = Version::parse(version_str) {
                major_packages
                    .entry(version.major)
                    .or_default()
                    .insert((*package_name).clone());
            }
        }
    }
    major_packages
        .into_iter()
        .max_by(|(left_major, left_pkgs), (right_major, right_pkgs)| {
            left_pkgs
                .len()
                .cmp(&right_pkgs.len())
                .then_with(|| left_major.cmp(right_major))
        })
        .map(|(major, _)| major)
}

fn preferred_version_in_major(
    versions: &BTreeSet<String>,
    target_major: Option<u64>,
) -> Option<String> {
    let target = target_major?;
    versions
        .iter()
        .filter_map(|v| Version::parse(v).ok().map(|parsed| (parsed, v)))
        .filter(|(parsed, _)| parsed.major == target)
        .max_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(_, raw)| raw.clone())
}

fn highest_version(versions: &BTreeSet<String>) -> Option<String> {
    versions
        .iter()
        .filter_map(|v| Version::parse(v).ok().map(|parsed| (parsed, v.clone())))
        .max_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(_, raw)| raw)
        .or_else(|| versions.iter().next_back().cloned())
}

#[cfg(test)]
mod scope_coherence_tests {
    use super::pick_scope_coherent_runtime_dependency_versions;
    use std::collections::{BTreeMap, BTreeSet};

    fn versions(strings: &[&str]) -> BTreeSet<String> {
        strings.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn single_package_picks_its_highest_version_regardless_of_scope() {
        let mut input = BTreeMap::new();
        input.insert("lodash".to_string(), versions(&["4.2.0", "4.17.21"]));
        let chosen = pick_scope_coherent_runtime_dependency_versions(&input);
        assert_eq!(chosen["lodash"], "4.17.21");
    }

    #[test]
    fn sentry_family_picks_a_single_major_shared_by_most_packages() {
        // Distribution captured from project 1: 4 packages have v7
        // attributions, only @sentry/browser has v8. Scope coherence must
        // align to v7 so the v7 packages stay installable as a coherent
        // family (the bundled http-integration crashes if mixed with v8 core).
        let mut input = BTreeMap::new();
        input.insert("@sentry/core".to_string(), versions(&["7.120.3", "8.55.0"]));
        input.insert("@sentry/node".to_string(), versions(&["7.120.4"]));
        input.insert("@sentry/tracing".to_string(), versions(&["7.120.4"]));
        input.insert("@sentry/utils".to_string(), versions(&["7.120.4"]));
        input.insert("@sentry/browser".to_string(), versions(&["8.55.0"]));
        let chosen = pick_scope_coherent_runtime_dependency_versions(&input);
        assert_eq!(
            chosen["@sentry/core"], "7.120.3",
            "core must drop to v7 to align with siblings"
        );
        assert_eq!(chosen["@sentry/node"], "7.120.4");
        assert_eq!(chosen["@sentry/tracing"], "7.120.4");
        assert_eq!(chosen["@sentry/utils"], "7.120.4");
        // browser has no v7 attribution; falls back to its own highest.
        // This still ships a mixed-major package.json but only one package
        // is incoherent — a strictly better failure mode than the old
        // "pick newest per package" behavior (which produced 1 coherent +
        // 4 broken).
        assert_eq!(chosen["@sentry/browser"], "8.55.0");
    }

    #[test]
    fn unscoped_packages_each_pick_their_highest_independently() {
        let mut input = BTreeMap::new();
        input.insert("lodash".to_string(), versions(&["4.17.21"]));
        input.insert("react".to_string(), versions(&["18.2.0"]));
        input.insert("zod".to_string(), versions(&["3.21.4"]));
        let chosen = pick_scope_coherent_runtime_dependency_versions(&input);
        assert_eq!(chosen.len(), 3);
        assert_eq!(chosen["lodash"], "4.17.21");
    }

    #[test]
    fn scope_with_tied_major_counts_breaks_tie_toward_higher_major() {
        // 1 pkg with v1-only and 1 pkg with v2-only — tie, pick higher.
        let mut input = BTreeMap::new();
        input.insert("@x/a".to_string(), versions(&["1.0.0"]));
        input.insert("@x/b".to_string(), versions(&["2.0.0"]));
        let chosen = pick_scope_coherent_runtime_dependency_versions(&input);
        assert_eq!(chosen["@x/a"], "1.0.0");
        assert_eq!(chosen["@x/b"], "2.0.0");
    }
}
