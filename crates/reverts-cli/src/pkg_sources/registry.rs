//! npm-registry HTTP client: packument fetch/parse, registry URL and
//! scoped-name encoding, integrity verification. Pure parsing/verification
//! logic is unit-tested with fixtures; only `http_get` touches the network.

use std::collections::{BTreeMap, BTreeSet};

use semver::Version;

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
}
