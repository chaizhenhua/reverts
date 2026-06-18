//! Cache-anchored package surface resolution: read a matched package's cached
//! `package.json` (carried in the loaded `PackageSource` set, stored normalized
//! as `export default {…}`), determine its real public export specifiers, and
//! emit accepted surfaces only for attributions whose specifier is proven
//! public. Pure logic; unit-tested with fixtures.

use std::collections::BTreeSet;

use reverts_input::{
    PackageAttributionInput, PackageAttributionStatus, PackageEmissionMode, PackageSurfaceInput,
};
use reverts_package::{PackageSourceCacheView, package_source_entry_path_from_source_path};

use crate::PackageSource;

fn package_source_cache_view(package_sources: &[PackageSource]) -> PackageSourceCacheView {
    let mut cache = PackageSourceCacheView::default();
    for source in package_sources {
        cache.insert_source(
            source.package_name.as_str(),
            source.package_version.as_str(),
            package_source_entry_path_from_source_path(
                source.package_name.as_str(),
                source.package_version.as_str(),
                source.source_path.as_str(),
            )
            .as_str(),
            source.source.as_str(),
        );
    }
    cache
}

/// Emit accepted external surfaces for matched packages, anchored on each
/// package's cached `package.json` public API. Only attributions whose
/// `export_specifier` is a proven public specifier produce a surface; internal
/// paths (e.g. `rxjs/internal/...`) never do.
pub(crate) fn resolve_cache_anchored_package_surfaces(
    attributions: &[PackageAttributionInput],
    package_sources: &[PackageSource],
    package_filter: Option<&BTreeSet<String>>,
) -> Vec<PackageSurfaceInput> {
    let cache = package_source_cache_view(package_sources);
    let mut emitted = BTreeSet::<String>::new();
    let mut surfaces = Vec::new();

    for attribution in attributions {
        if attribution.status != PackageAttributionStatus::Accepted
            || attribution.emission_mode != PackageEmissionMode::ExternalImport
        {
            continue;
        }
        if let Some(filter) = package_filter
            && !filter.contains(attribution.package_name.as_str())
        {
            continue;
        }
        let (Some(version), Some(specifier)) = (
            attribution.package_version.as_deref(),
            attribution.export_specifier.as_deref(),
        ) else {
            continue;
        };
        if emitted.contains(specifier) {
            continue;
        }
        if !cache.package_specifier_is_public(attribution.package_name.as_str(), version, specifier)
        {
            continue;
        }
        emitted.insert(specifier.to_string());
        surfaces.push(
            PackageSurfaceInput::accepted_external(
                attribution.package_name.clone(),
                version.to_string(),
                specifier.to_string(),
            )
            .with_evidence(format!("cache-anchored-public-export:{specifier}")),
        );
    }
    surfaces
}

/// Append cache-anchored package surfaces that are not already present in
/// `surfaces`.
///
/// Both the versioned matcher and the full ownership pipeline need this same
/// merge step: the matcher applies it after concrete matches, while the
/// pipeline applies it again after later ownership passes add more accepted
/// external-import attributions. Keeping the de-duplication here avoids two
/// subtly different "surface merge" implementations.
pub(crate) fn append_cache_anchored_package_surfaces(
    surfaces: &mut Vec<PackageSurfaceInput>,
    attributions: &[PackageAttributionInput],
    package_sources: &[PackageSource],
    package_filter: Option<&BTreeSet<String>>,
) {
    let mut existing_specifiers = surfaces
        .iter()
        .map(|surface| surface.export_specifier.clone())
        .collect::<BTreeSet<_>>();
    for surface in
        resolve_cache_anchored_package_surfaces(attributions, package_sources, package_filter)
    {
        if existing_specifiers.insert(surface.export_specifier.clone()) {
            surfaces.push(surface);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reverts_ir::ModuleId;

    fn pkg_json_source(name: &str, version: &str, body_json: &str) -> PackageSource {
        // Real loaded/materialized sources carry `{name}@{version}/{rel_path}`
        // as source_path — the root manifest is NOT bare "package.json".
        PackageSource::source_only(
            name,
            version,
            name,
            format!("{name}@{version}/package.json"),
            format!("export default {body_json};"),
        )
    }

    fn accepted(
        module: u32,
        name: &str,
        version: &str,
        specifier: &str,
    ) -> PackageAttributionInput {
        PackageAttributionInput::accepted_external(ModuleId(module), name, version, specifier)
    }

    #[test]
    fn emits_surface_for_public_specifier_only() {
        let sources = vec![pkg_json_source(
            "rxjs",
            "7.8.1",
            r#"{"name":"rxjs","exports":{".":"./d/index.js","./operators":"./d/op.js"}}"#,
        )];
        let attrs = vec![
            accepted(1, "rxjs", "7.8.1", "rxjs"),
            accepted(2, "rxjs", "7.8.1", "rxjs/operators"),
            accepted(3, "rxjs", "7.8.1", "rxjs/internal/util/isFunction"),
        ];
        let surfaces = resolve_cache_anchored_package_surfaces(&attrs, &sources, None);
        let specs = surfaces
            .iter()
            .map(|s| s.export_specifier.clone())
            .collect::<BTreeSet<_>>();
        assert!(specs.contains("rxjs"));
        assert!(specs.contains("rxjs/operators"));
        assert!(!specs.contains("rxjs/internal/util/isFunction"));
        assert_eq!(surfaces.len(), 2);
        assert!(
            surfaces
                .iter()
                .all(|s| s.status == PackageAttributionStatus::Accepted)
        );
    }

    #[test]
    fn no_surface_when_package_json_absent() {
        let attrs = vec![accepted(1, "zod", "3.0.0", "zod")];
        assert!(resolve_cache_anchored_package_surfaces(&attrs, &[], None).is_empty());
    }

    #[test]
    fn cache_anchored_surfaces_use_the_attribution_package_version() {
        let sources = vec![
            pkg_json_source(
                "pkg",
                "1.0.0",
                r#"{"name":"pkg","exports":{".":"./index.js","./internal":"./internal.js"}}"#,
            ),
            pkg_json_source(
                "pkg",
                "2.0.0",
                r#"{"name":"pkg","exports":{".":"./index.js"}}"#,
            ),
        ];
        let attrs = vec![
            accepted(1, "pkg", "2.0.0", "pkg/internal"),
            accepted(2, "pkg", "1.0.0", "pkg/internal"),
        ];

        let surfaces = resolve_cache_anchored_package_surfaces(&attrs, &sources, None);

        assert_eq!(surfaces.len(), 1);
        assert_eq!(surfaces[0].package_version.as_deref(), Some("1.0.0"));
        assert_eq!(surfaces[0].export_specifier, "pkg/internal");
    }
}
