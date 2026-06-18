//! Cache-anchored package surface resolution: read a matched package's cached
//! `package.json` (carried in the loaded `PackageSource` set, stored normalized
//! as `export default {…}`), determine its real public export specifiers, and
//! emit accepted surfaces only for attributions whose specifier is proven
//! public. Pure logic; unit-tested with fixtures.

use std::collections::{BTreeMap, BTreeSet};

use reverts_input::{
    PackageAttributionInput, PackageAttributionStatus, PackageEmissionMode, PackageSurfaceInput,
};

use crate::PackageSource;

/// Parse a cached `package.json` body into a JSON object. The cache stores it
/// normalized as `export default {…};`, so strip that wrapper before parsing.
pub(crate) fn parse_cached_package_json(source: &str) -> Option<serde_json::Value> {
    let trimmed = source.trim();
    let body = trimmed
        .strip_prefix("export default")
        .map(str::trim_start)
        .unwrap_or(trimmed);
    let body = body.trim().trim_end_matches(';').trim();
    serde_json::from_str::<serde_json::Value>(body).ok()
}

/// The path of a source relative to its package root. Loaded/materialized
/// sources carry `source_path == "{name}@{version}/{rel_path}"`, so the root
/// `package.json` appears as e.g. `rxjs@7.8.1/package.json`, not `package.json`.
fn source_rel_path(source: &PackageSource) -> Option<&str> {
    let prefix = format!("{}@{}/", source.package_name, source.package_version);
    source.source_path.strip_prefix(prefix.as_str())
}

/// The cached root `package.json` value for `package_name`, if present in the
/// loaded sources (its rel path is exactly `package.json`).
pub(crate) fn cached_root_package_json(
    package_name: &str,
    package_sources: &[PackageSource],
) -> Option<serde_json::Value> {
    package_sources
        .iter()
        .find(|src| {
            src.package_name == package_name && source_rel_path(src) == Some("package.json")
        })
        .and_then(|src| parse_cached_package_json(src.source.as_str()))
}

/// Public entry point: whether `specifier` is a publicly-importable specifier of
/// `package_name`, given the package's cached `package.json` source (which may be
/// stored `export default`-wrapped) and whether it ships a root index file.
/// Returns `false` when the manifest cannot be parsed — conservative: a specifier
/// we cannot prove public must not be externalized into a bare import that may
/// fail to resolve at runtime. Used by externalization passes outside this crate
/// to avoid emitting imports for non-public subpaths (e.g. `axios/exports`).
pub fn package_specifier_is_public(
    package_json_source: &str,
    package_name: &str,
    specifier: &str,
    has_root_index: bool,
) -> bool {
    match parse_cached_package_json(package_json_source) {
        Some(package_json) => {
            specifier_is_public(package_name, &package_json, specifier, has_root_index)
        }
        None => false,
    }
}

/// Whether `specifier` is a *publicly resolvable* import of the package, per
/// Node resolution applied to the cached `package.json`:
/// - `exports` subpath map: the specifier must match an exact key or a
///   `./prefix/*` pattern whose target is non-null (an explicit allowlist).
/// - `exports` string / root conditions object: only the bare root is public.
/// - no `exports` field: the bare root is public when `main`/`module` is
///   declared or an `index` file exists (Node's default entry); any subpath is
///   resolvable (Node resolves any existing file; the matcher already proved the
///   file exists for this specifier). `has_index` reports whether the package
///   ships a root `index.{js,json,node}`.
pub(crate) fn specifier_is_public(
    package_name: &str,
    package_json: &serde_json::Value,
    specifier: &str,
    has_index: bool,
) -> bool {
    let subpath = if specifier == package_name {
        ".".to_string()
    } else if let Some(rest) = specifier.strip_prefix(package_name) {
        match rest.strip_prefix('/') {
            Some(sub) if !sub.is_empty() => format!("./{sub}"),
            _ => return false,
        }
    } else {
        return false;
    };

    match package_json.get("exports") {
        Some(serde_json::Value::Object(map)) => {
            if map.keys().any(|key| key == "." || key.starts_with("./")) {
                exports_subpath_is_public(map, &subpath)
            } else {
                // Root-only conditions object (e.g. {import, require}).
                subpath == "."
            }
        }
        Some(serde_json::Value::String(_)) => subpath == ".",
        _ => {
            if subpath == "." {
                package_json.get("main").is_some()
                    || package_json.get("module").is_some()
                    || has_index
            } else {
                // No `exports` allowlist: any existing file is importable.
                true
            }
        }
    }
}

/// Whether the package ships a root default-entry file (`index.{js,json,node}`),
/// which Node resolves for a bare import when `main`/`module` is absent.
fn package_has_root_index(package_name: &str, package_sources: &[PackageSource]) -> bool {
    package_sources.iter().any(|src| {
        src.package_name == package_name
            && matches!(
                source_rel_path(src),
                Some("index.js" | "index.json" | "index.node")
            )
    })
}

/// Whether `subpath` (e.g. `./operators` or `./internal/util/x.js`) is exposed
/// by an `exports` subpath map: an exact non-null key, or a `./prefix/*` pattern
/// whose target is non-null.
fn exports_subpath_is_public(
    map: &serde_json::Map<String, serde_json::Value>,
    subpath: &str,
) -> bool {
    if let Some(target) = map.get(subpath) {
        return !target.is_null();
    }
    map.iter().any(|(key, target)| {
        key.strip_suffix('*').is_some_and(|prefix| {
            subpath.len() > prefix.len() && subpath.starts_with(prefix) && !target.is_null()
        })
    })
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
    let mut package_meta_by_name = BTreeMap::<String, (Option<serde_json::Value>, bool)>::new();
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
        let (package_json, has_index) = package_meta_by_name
            .entry(attribution.package_name.clone())
            .or_insert_with(|| {
                (
                    cached_root_package_json(attribution.package_name.as_str(), package_sources),
                    package_has_root_index(attribution.package_name.as_str(), package_sources),
                )
            });
        let Some(package_json) = package_json.as_ref() else {
            continue;
        };
        if !specifier_is_public(
            attribution.package_name.as_str(),
            package_json,
            specifier,
            *has_index,
        ) {
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

    fn json(s: &str) -> serde_json::Value {
        parse_cached_package_json(s).expect("parse")
    }

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
    fn specifier_is_public_matches_exact_and_wildcard_exports() {
        let pj = json(
            r#"{"name":"rxjs","exports":{".":"./d/index.js","./operators":"./d/op.js","./fetch/*":"./d/fetch/*.js"}}"#,
        );
        assert!(specifier_is_public("rxjs", &pj, "rxjs", false));
        assert!(specifier_is_public("rxjs", &pj, "rxjs/operators", false));
        assert!(specifier_is_public("rxjs", &pj, "rxjs/fetch/client", false));
        // `./internal/*` is not exported, so internal paths stay non-public.
        assert!(!specifier_is_public(
            "rxjs",
            &pj,
            "rxjs/internal/util/isFunction",
            false
        ));
    }

    #[test]
    fn specifier_is_public_rejects_null_blocked_exports() {
        let pj = json(r#"{"name":"p","exports":{".":"./i.js","./internal/*":null}}"#);
        assert!(specifier_is_public("p", &pj, "p", false));
        assert!(!specifier_is_public("p", &pj, "p/internal/secret", false));
    }

    #[test]
    fn specifier_is_public_root_only_for_conditions_or_string_exports() {
        let conditions = json(r#"{"name":"ws","exports":{"import":"./w.mjs","require":"./w.js"}}"#);
        assert!(specifier_is_public("ws", &conditions, "ws", false));
        assert!(!specifier_is_public("ws", &conditions, "ws/lib/x", false));
        let string_exports = json(r#"{"name":"ws","exports":"./w.js"}"#);
        assert!(specifier_is_public("ws", &string_exports, "ws", false));
        assert!(!specifier_is_public(
            "ws",
            &string_exports,
            "ws/lib/x",
            false
        ));
    }

    #[test]
    fn specifier_is_public_allows_deep_imports_without_exports_field() {
        // No `exports` allowlist → any existing file is importable (Node CJS).
        let pj = json(r#"{"name":"semver","main":"./index.js"}"#);
        assert!(specifier_is_public("semver", &pj, "semver", false));
        assert!(specifier_is_public(
            "semver",
            &pj,
            "semver/classes/range.js",
            false
        ));
        // No main/module/exports: the bare root is public only when an index
        // file exists (Node's default entry); deep imports resolve regardless.
        let no_entry = json(r#"{"name":"semver"}"#);
        assert!(!specifier_is_public("semver", &no_entry, "semver", false));
        assert!(specifier_is_public("semver", &no_entry, "semver", true));
        assert!(specifier_is_public(
            "semver",
            &no_entry,
            "semver/classes/range.js",
            false
        ));
    }

    #[test]
    fn parses_export_default_wrapped_package_json() {
        let body =
            r#"export default {"name":"rxjs","version":"7.8.1","main":"./dist/cjs/index.js"};"#;
        let value = parse_cached_package_json(body).expect("parse");
        assert_eq!(value.get("name").and_then(|v| v.as_str()), Some("rxjs"));
        assert_eq!(
            value.get("main").and_then(|v| v.as_str()),
            Some("./dist/cjs/index.js")
        );
    }

    #[test]
    fn parses_plain_json_without_wrapper() {
        let value = parse_cached_package_json(r#"{"name":"x","version":"1.0.0"}"#).expect("parse");
        assert_eq!(value.get("name").and_then(|v| v.as_str()), Some("x"));
    }
}
