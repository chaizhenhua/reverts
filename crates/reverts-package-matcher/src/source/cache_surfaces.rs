//! Cache-anchored package surface resolution: read a matched package's cached
//! `package.json` (carried in the loaded `PackageSource` set, stored normalized
//! as `export default {…}`), determine its real public export specifiers, and
//! emit accepted surfaces only for attributions whose specifier is proven
//! public. Pure logic; unit-tested with fixtures.

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

#[cfg(test)]
mod tests {
    use super::*;

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
