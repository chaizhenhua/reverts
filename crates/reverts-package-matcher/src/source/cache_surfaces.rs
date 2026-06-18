//! Cache-anchored package surface resolution: read a matched package's cached
//! `package.json` (carried in the loaded `PackageSource` set, stored normalized
//! as `export default {…}`), determine its real public export specifiers, and
//! emit accepted surfaces only for attributions whose specifier is proven
//! public. Pure logic; unit-tested with fixtures.

use std::collections::BTreeSet;

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

/// The cached root `package.json` value for `package_name`, if present in the
/// loaded sources (root entry has `source_path == "package.json"`).
pub(crate) fn cached_root_package_json(
    package_name: &str,
    package_sources: &[PackageSource],
) -> Option<serde_json::Value> {
    package_sources
        .iter()
        .find(|src| src.package_name == package_name && src.source_path == "package.json")
        .and_then(|src| parse_cached_package_json(src.source.as_str()))
}

/// The set of public import specifiers a package exposes, derived from its
/// `package.json`. Always specifier strings (e.g. `rxjs`, `rxjs/operators`).
pub(crate) fn public_export_specifiers(
    package_name: &str,
    package_json: &serde_json::Value,
) -> BTreeSet<String> {
    let mut specifiers = BTreeSet::new();
    match package_json.get("exports") {
        Some(serde_json::Value::Object(map)) => {
            let looks_like_subpath_map = map.keys().any(|k| k == "." || k.starts_with("./"));
            if looks_like_subpath_map {
                for key in map.keys() {
                    if key.contains('*') {
                        continue;
                    }
                    if key == "." {
                        specifiers.insert(package_name.to_string());
                    } else if let Some(sub) = key.strip_prefix("./") {
                        if !sub.is_empty() {
                            specifiers.insert(format!("{package_name}/{sub}"));
                        }
                    }
                }
            } else {
                // `exports` is a conditions object for the root (e.g. {import,require}).
                specifiers.insert(package_name.to_string());
            }
        }
        Some(serde_json::Value::String(_)) => {
            // `exports: "./index.js"` — root only.
            specifiers.insert(package_name.to_string());
        }
        _ => {
            // No `exports`: the root is importable if main/module is declared.
            // Phase 1 only asserts the root as public to stay safe.
            if package_json.get("main").is_some() || package_json.get("module").is_some() {
                specifiers.insert(package_name.to_string());
            }
        }
    }
    specifiers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(s: &str) -> serde_json::Value {
        parse_cached_package_json(s).expect("parse")
    }

    #[test]
    fn subpath_exports_map_yields_root_and_subpaths() {
        let pj = json(
            r#"{"name":"rxjs","exports":{".":"./d/index.js","./operators":"./d/op.js","./internal/*":"./d/internal/*.js"}}"#,
        );
        let s = public_export_specifiers("rxjs", &pj);
        assert!(s.contains("rxjs"));
        assert!(s.contains("rxjs/operators"));
        assert!(!s.iter().any(|x| x.contains('*')));
        assert!(!s.contains("rxjs/internal"));
    }

    #[test]
    fn root_conditions_object_yields_bare_only() {
        let pj = json(r#"{"name":"ws","exports":{"import":"./w.mjs","require":"./w.js"}}"#);
        let s = public_export_specifiers("ws", &pj);
        assert_eq!(s.into_iter().collect::<Vec<_>>(), vec!["ws".to_string()]);
    }

    #[test]
    fn no_exports_falls_back_to_root_when_main_present() {
        let pj = json(r#"{"name":"semver","main":"./index.js"}"#);
        let s = public_export_specifiers("semver", &pj);
        assert_eq!(
            s.into_iter().collect::<Vec<_>>(),
            vec!["semver".to_string()]
        );
        let pj2 = json(r#"{"name":"semver"}"#);
        assert!(public_export_specifiers("semver", &pj2).is_empty());
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
