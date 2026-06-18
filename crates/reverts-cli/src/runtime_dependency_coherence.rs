//! Final `package.json` coherence prune for externalized runtime deps.
//!
//! The pipeline's scope-coherence vote (`reverts_pipeline::runtime_dependencies`)
//! aligns each `@scope/*` family to a winning major where it can, but a sibling
//! the matcher only ever fingerprinted to an off-major cached version keeps that
//! wrong-major pin. Root-pinning several majors of one scope — e.g.
//! `@smithy/core@2.0.0` next to the `@smithy@3` family that the pinned
//! `@aws-sdk@3.x` packages require — makes npm's arborist place duplicate,
//! conflicting copies at many tree depths and blow up during `npm install`
//! (it exits non-zero mid-resolution with no usable diagnostic).
//!
//! Those off-major siblings are transitive dependencies of the retained,
//! coherent packages, so npm already installs a coherent copy on its own. This
//! pass drops such a sibling from the root `package.json` *only* when it is in
//! the transitive dependency closure of the retained packages — proven from the
//! cached `package.json` dependency graph — so the generated bare import still
//! resolves (to the coherent, transitively-installed copy). A scope-incoherent
//! package that nothing else pulls (for example project 1's lone `@sentry/browser`
//! v8 among a v7 family) is kept, since dropping it would strand its import.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use reverts_package::parse_package_json_source;
use reverts_pipeline::RuntimeDependency;
use semver::{Version, VersionReq};

use crate::persistence::source_cache::MaterializedPackageManifests;

/// Drop scope-incoherent runtime dependencies that npm will install
/// transitively at a coherent version anyway. See module docs for the why.
pub(crate) fn prune_transitively_provided_scope_incoherent_dependencies(
    dependencies: Vec<RuntimeDependency>,
    manifests: &MaterializedPackageManifests,
) -> Vec<RuntimeDependency> {
    let dominant = scope_dominant_majors(&dependencies);
    // Retain a dependency unless it is a scoped sibling whose major disagrees
    // with the major most of its scope landed on.
    let (mut retained, candidates): (Vec<_>, Vec<_>) =
        dependencies.into_iter().partition(|dependency| {
            match (
                scope_prefix(&dependency.package_name),
                dependency_major(dependency),
            ) {
                (Some(scope), Some(major)) => {
                    dominant.get(&scope).is_none_or(|winning| *winning == major)
                }
                _ => true,
            }
        });
    if candidates.is_empty() {
        return retained;
    }

    let provided = transitively_provided_package_names(&retained, manifests);
    for candidate in candidates {
        // Keep the off-major pin only when nothing else would install it; a
        // transitively-provided sibling is redundant and harmful at the root.
        if !provided.contains(&candidate.package_name) {
            retained.push(candidate);
        }
    }
    retained.sort_by(|left, right| left.package_name.cmp(&right.package_name));
    retained
}

/// For every `@scope` with at least two packages, the major the most packages
/// landed on (ties resolved toward the higher major), mirroring the
/// pipeline-side scope-coherence vote.
fn scope_dominant_majors(dependencies: &[RuntimeDependency]) -> BTreeMap<String, u64> {
    let mut by_scope: BTreeMap<String, BTreeMap<u64, BTreeSet<String>>> = BTreeMap::new();
    for dependency in dependencies {
        if let (Some(scope), Some(major)) = (
            scope_prefix(&dependency.package_name),
            dependency_major(dependency),
        ) {
            by_scope
                .entry(scope)
                .or_default()
                .entry(major)
                .or_default()
                .insert(dependency.package_name.clone());
        }
    }
    by_scope
        .into_iter()
        .filter_map(|(scope, majors)| {
            let package_count: usize = majors.values().map(BTreeSet::len).sum();
            if package_count < 2 {
                return None;
            }
            majors
                .into_iter()
                .max_by(
                    |(left_major, left_packages), (right_major, right_packages)| {
                        left_packages
                            .len()
                            .cmp(&right_packages.len())
                            .then_with(|| left_major.cmp(right_major))
                    },
                )
                .map(|(major, _)| (scope, major))
        })
        .collect()
}

/// Names npm would install transitively from the retained packages. A name is
/// "provided" the moment it is listed as a dependency of an already-installed
/// package, so the result is sound even when the dependency's own version was
/// never fetched into the cache (npm fetches it from the registry).
fn transitively_provided_package_names(
    retained: &[RuntimeDependency],
    manifests: &MaterializedPackageManifests,
) -> BTreeSet<String> {
    let mut dependency_ranges: BTreeMap<(String, String), BTreeMap<String, String>> =
        BTreeMap::new();
    let mut versions_by_name: BTreeMap<String, Vec<Version>> = BTreeMap::new();
    for ((name, version), manifest) in manifests.packages() {
        if let Ok(parsed) = Version::parse(version.trim()) {
            versions_by_name
                .entry(name.to_string())
                .or_default()
                .push(parsed);
        }
        let Some(package_json_source) = manifest.package_json_source() else {
            continue;
        };
        dependency_ranges.insert(
            (name.to_string(), version.to_string()),
            package_json_dependency_ranges(package_json_source),
        );
    }
    for versions in versions_by_name.values_mut() {
        versions.sort();
        versions.dedup();
    }

    let mut provided = BTreeSet::new();
    let mut visited: BTreeSet<(String, String)> = BTreeSet::new();
    let mut queue: VecDeque<(String, String)> = retained
        .iter()
        .map(|dependency| {
            (
                dependency.package_name.clone(),
                dependency.package_version.trim().to_string(),
            )
        })
        .collect();
    while let Some(node) = queue.pop_front() {
        if !visited.insert(node.clone()) {
            continue;
        }
        let Some(ranges) = dependency_ranges.get(&node) else {
            // We can only traverse through packages whose manifest is cached;
            // a node we cannot open simply does not extend the closure.
            continue;
        };
        for (dependency_name, range) in ranges {
            provided.insert(dependency_name.clone());
            if let Some(resolved) =
                resolve_highest_satisfying(dependency_name, range, &versions_by_name)
            {
                queue.push_back((dependency_name.clone(), resolved));
            }
        }
    }
    provided
}

/// Merge `dependencies`, `optionalDependencies`, and `peerDependencies` from a
/// cached `package.json` source into name → range pairs.
fn package_json_dependency_ranges(source: &str) -> BTreeMap<String, String> {
    let mut ranges = BTreeMap::new();
    let Some(value) = parse_package_json_source(source) else {
        return ranges;
    };
    for field in ["dependencies", "optionalDependencies", "peerDependencies"] {
        let Some(object) = value.get(field).and_then(serde_json::Value::as_object) else {
            continue;
        };
        for (name, range) in object {
            if let Some(range) = range.as_str() {
                ranges
                    .entry(name.clone())
                    .or_insert_with(|| range.to_string());
            }
        }
    }
    ranges
}

/// Highest cached version of `name` satisfying `range`, mirroring npm's greedy
/// resolution. Unparseable ranges (git URLs, `workspace:*`, …) yield `None`,
/// which only stops the closure from traversing *through* that node — the
/// dependency name is still recorded as provided by the caller.
fn resolve_highest_satisfying(
    name: &str,
    range: &str,
    versions_by_name: &BTreeMap<String, Vec<Version>>,
) -> Option<String> {
    let requirement = VersionReq::parse(range).ok()?;
    versions_by_name
        .get(name)?
        .iter()
        .rev()
        .find(|version| requirement.matches(version))
        .map(Version::to_string)
}

fn scope_prefix(package_name: &str) -> Option<String> {
    let rest = package_name.strip_prefix('@')?;
    let slash = rest.find('/')?;
    Some(format!("@{}", &rest[..slash]))
}

fn dependency_major(dependency: &RuntimeDependency) -> Option<u64> {
    Version::parse(dependency.package_version.trim())
        .ok()
        .map(|version| version.major)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dep(name: &str, version: &str) -> RuntimeDependency {
        RuntimeDependency {
            package_name: name.to_string(),
            package_version: version.to_string(),
        }
    }

    fn insert_manifest(
        manifests: &mut MaterializedPackageManifests,
        name: &str,
        version: &str,
        deps: &[(&str, &str)],
    ) {
        let body = deps
            .iter()
            .map(|(name, range)| format!("\"{name}\":\"{range}\""))
            .collect::<Vec<_>>()
            .join(",");
        manifests.insert_source(
            name,
            version,
            "package.json",
            format!("export default {{\"dependencies\":{{{body}}}}}"),
        );
    }

    #[test]
    fn drops_off_major_scope_sibling_that_is_transitively_provided() {
        // @aws-sdk/core@3 depends on @smithy/core; the matcher mis-pinned
        // @smithy/core to a v2 cached version while the rest of @smithy is v3.
        let dependencies = vec![
            dep("@aws-sdk/client-sso", "3.980.0"),
            dep("@aws-sdk/core", "3.973.2"),
            dep("@smithy/types", "3.7.2"),
            dep("@smithy/protocol-http", "3.3.0"),
            dep("@smithy/core", "2.0.0"),
        ];
        let mut manifests = MaterializedPackageManifests::new();
        insert_manifest(
            &mut manifests,
            "@aws-sdk/client-sso",
            "3.980.0",
            &[("@aws-sdk/core", "3.973.2")],
        );
        insert_manifest(
            &mut manifests,
            "@aws-sdk/core",
            "3.973.2",
            &[("@smithy/core", "^3.1.0"), ("@smithy/types", "^3.7.2")],
        );

        let pruned =
            prune_transitively_provided_scope_incoherent_dependencies(dependencies, &manifests);
        let names: Vec<&str> = pruned.iter().map(|d| d.package_name.as_str()).collect();
        assert!(
            !names.contains(&"@smithy/core"),
            "off-major @smithy/core is provided transitively by @aws-sdk/core; drop it: {names:?}"
        );
        assert!(names.contains(&"@smithy/types"), "v3 siblings stay pinned");
        assert!(names.contains(&"@aws-sdk/core"));
    }

    #[test]
    fn keeps_off_major_scope_sibling_that_nothing_provides() {
        // Project 1's shape: a v7 @sentry family plus a lone v8 @sentry/browser
        // that no retained package depends on. Dropping it would strand the
        // generated import, so it must stay (mixed-major but installable).
        let dependencies = vec![
            dep("@sentry/core", "7.120.3"),
            dep("@sentry/node", "7.120.4"),
            dep("@sentry/tracing", "7.120.4"),
            dep("@sentry/browser", "8.55.0"),
        ];
        let mut manifests = MaterializedPackageManifests::new();
        insert_manifest(
            &mut manifests,
            "@sentry/node",
            "7.120.4",
            &[("@sentry/core", "7.120.4")],
        );
        insert_manifest(&mut manifests, "@sentry/core", "7.120.3", &[]);

        let pruned =
            prune_transitively_provided_scope_incoherent_dependencies(dependencies, &manifests);
        let names: Vec<&str> = pruned.iter().map(|d| d.package_name.as_str()).collect();
        assert!(
            names.contains(&"@sentry/browser"),
            "browser is not transitively provided, keep it: {names:?}"
        );
        assert_eq!(pruned.len(), 4);
    }

    #[test]
    fn leaves_coherent_single_major_scope_untouched() {
        let dependencies = vec![
            dep("@aws-sdk/client-sso", "3.980.0"),
            dep("@aws-sdk/core", "3.973.2"),
        ];
        let pruned = prune_transitively_provided_scope_incoherent_dependencies(
            dependencies.clone(),
            &MaterializedPackageManifests::new(),
        );
        assert_eq!(pruned.len(), 2);
    }

    #[test]
    fn unscoped_dependencies_are_never_dropped() {
        let dependencies = vec![dep("lodash", "4.17.21"), dep("react", "18.2.0")];
        let pruned = prune_transitively_provided_scope_incoherent_dependencies(
            dependencies,
            &MaterializedPackageManifests::new(),
        );
        assert_eq!(pruned.len(), 2);
    }
}
