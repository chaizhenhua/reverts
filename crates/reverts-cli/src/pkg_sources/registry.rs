//! npm-registry HTTP client: packument fetch/parse, registry URL and
//! scoped-name encoding, integrity verification. Pure parsing/verification
//! logic is unit-tested with fixtures; only `http_get` touches the network.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::time::Duration;

use base64::Engine as _;
use semver::Version;
use sha2::{Digest, Sha512};

use crate::errors::MatchPackagesError;

/// A single resolved version entry from a packument's `versions` map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackumentVersion {
    pub(crate) tarball: String,
    /// `dist.integrity`, e.g. `sha512-<base64>`. Absent on very old packages.
    pub(crate) integrity: Option<String>,
}

/// The subset of a packument we consume.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Packument {
    pub(crate) versions: BTreeMap<String, PackumentVersion>,
}

impl Packument {
    /// Semver-parseable version keys, for range resolution.
    pub(crate) fn available_versions(&self) -> BTreeSet<Version> {
        self.versions
            .keys()
            .filter_map(|raw| Version::parse(raw).ok())
            .collect()
    }
}

/// Parse a packument JSON document into the subset we consume.
pub(crate) fn parse_packument(
    package_name: &str,
    bytes: &[u8],
) -> Result<Packument, MatchPackagesError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|source| MatchPackagesError::ParsePackument {
            package_name: package_name.to_string(),
            message: format!("invalid JSON: {source}"),
        })?;
    let versions_obj = value.get("versions").and_then(serde_json::Value::as_object);
    let Some(versions_obj) = versions_obj else {
        return Err(MatchPackagesError::ParsePackument {
            package_name: package_name.to_string(),
            message: "packument has no `versions` object".to_string(),
        });
    };
    let mut versions = BTreeMap::new();
    for (version, entry) in versions_obj {
        let Some(dist) = entry.get("dist") else {
            continue;
        };
        let Some(tarball) = dist.get("tarball").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let integrity = dist
            .get("integrity")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        versions.insert(
            version.clone(),
            PackumentVersion {
                tarball: tarball.to_string(),
                integrity,
            },
        );
    }
    Ok(Packument { versions })
}

const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";

/// Registry base URL: `REVERTS_NPM_REGISTRY` or the public default. Trailing
/// slash trimmed so URL joins are predictable.
pub(crate) fn registry_base_url() -> String {
    let raw = std::env::var("REVERTS_NPM_REGISTRY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REGISTRY.to_string());
    raw.trim_end_matches('/').to_string()
}

/// Build the packument URL. Scoped names (`@scope/name`) keep the `@scope/`
/// segment but URL-encode the internal `/` of the name per the registry API
/// (`@scope%2fname`).
pub(crate) fn packument_url(base: &str, package_name: &str) -> String {
    let base = base.trim_end_matches('/');
    if let Some(rest) = package_name.strip_prefix('@') {
        if let Some((scope, name)) = rest.split_once('/') {
            return format!("{base}/@{scope}%2f{name}");
        }
    }
    format!("{base}/{package_name}")
}

/// Verify a tarball against a `dist.integrity` string. Only `sha512-<base64>`
/// is supported (the registry's current default). A missing/empty integrity
/// is rejected — we never trust unverifiable bytes.
pub(crate) fn verify_integrity(
    package_name: &str,
    package_version: &str,
    tarball: &[u8],
    integrity: Option<&str>,
) -> Result<(), MatchPackagesError> {
    let make_err = |message: String| MatchPackagesError::PackageSourceIntegrity {
        package_name: package_name.to_string(),
        package_version: package_version.to_string(),
        message,
    };
    let integrity = integrity
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| make_err("registry provided no integrity hash".to_string()))?;
    let Some(b64) = integrity.strip_prefix("sha512-") else {
        return Err(make_err(format!(
            "unsupported integrity algorithm: {integrity}"
        )));
    };
    let expected = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|source| make_err(format!("invalid base64 integrity: {source}")))?;
    let actual = Sha512::digest(tarball);
    if actual.as_slice() != expected.as_slice() {
        return Err(make_err("sha512 mismatch".to_string()));
    }
    Ok(())
}

/// GET raw bytes from `url`, attaching `Authorization: Bearer` when
/// `REVERTS_NPM_TOKEN` is set.
pub(crate) fn http_get(url: &str) -> Result<Vec<u8>, MatchPackagesError> {
    let make_err = |message: String| MatchPackagesError::RegistryRequest {
        url: url.to_string(),
        message,
    };
    let mut request = ureq::get(url).timeout(Duration::from_secs(http_timeout_secs()));
    if let Ok(token) = std::env::var("REVERTS_NPM_TOKEN") {
        if !token.trim().is_empty() {
            request = request.set("Authorization", &format!("Bearer {token}"));
        }
    }
    let response = request
        .call()
        .map_err(|source| make_err(source.to_string()))?;
    let mut buffer = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut buffer)
        .map_err(|source| make_err(format!("failed to read response body: {source}")))?;
    Ok(buffer)
}

fn http_timeout_secs() -> u64 {
    std::env::var("REVERTS_NPM_HTTP_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(120)
}

/// Fetch and parse the packument for `package_name` from the configured registry.
pub(crate) fn fetch_packument(package_name: &str) -> Result<Packument, MatchPackagesError> {
    let url = packument_url(&registry_base_url(), package_name);
    let bytes = http_get(&url)?;
    parse_packument(package_name, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_packument_extracts_versions_and_dist() {
        let json = br#"{
            "name": "left-pad",
            "versions": {
                "1.0.0": { "dist": { "tarball": "https://r/left-pad-1.0.0.tgz", "integrity": "sha512-AAAA" } },
                "1.1.0": { "dist": { "tarball": "https://r/left-pad-1.1.0.tgz" } },
                "bad":   { "dist": { "tarball": "https://r/left-pad-bad.tgz" } }
            }
        }"#;
        let packument = parse_packument("left-pad", json).expect("parse");
        assert_eq!(packument.versions.len(), 3);
        let v100 = packument.versions.get("1.0.0").expect("1.0.0");
        assert_eq!(v100.tarball, "https://r/left-pad-1.0.0.tgz");
        assert_eq!(v100.integrity.as_deref(), Some("sha512-AAAA"));
        assert_eq!(packument.versions["1.1.0"].integrity, None);
        // "bad" is not semver-parseable, so it is excluded from available_versions only.
        let available = packument.available_versions();
        assert_eq!(available.len(), 2);
        assert!(available.contains(&Version::parse("1.1.0").expect("ver")));
    }

    #[test]
    fn parse_packument_without_versions_errors() {
        let err = parse_packument("x", br#"{"name":"x"}"#).expect_err("should error");
        assert!(matches!(err, MatchPackagesError::ParsePackument { .. }));
    }

    #[test]
    fn packument_url_for_unscoped_package() {
        assert_eq!(
            packument_url("https://registry.npmjs.org", "left-pad"),
            "https://registry.npmjs.org/left-pad"
        );
    }

    #[test]
    fn packument_url_encodes_scoped_package() {
        assert_eq!(
            packument_url("https://registry.npmjs.org/", "@scope/name"),
            "https://registry.npmjs.org/@scope%2fname"
        );
    }

    #[test]
    fn verify_integrity_accepts_matching_sha512() {
        let data = b"hello tarball";
        let digest = Sha512::digest(data);
        let integrity = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(digest)
        );
        assert!(verify_integrity("p", "1.0.0", data, Some(&integrity)).is_ok());
    }

    #[test]
    fn verify_integrity_rejects_mismatch_and_missing() {
        let integrity = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(Sha512::digest(b"other"))
        );
        assert!(matches!(
            verify_integrity("p", "1.0.0", b"hello", Some(&integrity)),
            Err(MatchPackagesError::PackageSourceIntegrity { .. })
        ));
        assert!(verify_integrity("p", "1.0.0", b"hello", None).is_err());
        assert!(verify_integrity("p", "1.0.0", b"hello", Some("sha1-abc")).is_err());
    }
}
