//! Fixed on-disk package source cache at `~/.reverts/package-cache`
//! (override: `REVERTS_PACKAGE_CACHE_DIR`). Entries are immutable, keyed by
//! `<registry-host>/<pkg>/<version>/`, holding the verified `.tgz`, a
//! `meta.json`, and the extracted `package/` tree.

use std::path::{Path, PathBuf};

use crate::errors::MatchPackagesError;

/// Resolve the cache root: `REVERTS_PACKAGE_CACHE_DIR` if set, else
/// `$HOME/.reverts/package-cache`.
pub(crate) fn cache_root() -> Result<PathBuf, MatchPackagesError> {
    if let Ok(dir) = std::env::var("REVERTS_PACKAGE_CACHE_DIR") {
        if !dir.trim().is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    let home = std::env::var("HOME").map_err(|_| MatchPackagesError::ResolveCacheDir {
        message: "HOME is not set and REVERTS_PACKAGE_CACHE_DIR is unset".to_string(),
    })?;
    Ok(PathBuf::from(home).join(".reverts").join("package-cache"))
}

/// Host[:port] of a registry base URL, sanitized for use as a path segment.
/// Falls back to a sanitized form of the whole base when no host is found.
pub(crate) fn registry_host(base_url: &str) -> String {
    let without_scheme = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    let authority = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme);
    let sanitized: String = authority
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "unknown-registry".to_string()
    } else {
        sanitized
    }
}

/// Directory holding the cache entry for one package version (the parent of
/// `package.tgz`, `meta.json`, and `package/`).
pub(crate) fn entry_dir(
    root: &Path,
    registry_host: &str,
    package_name: &str,
    version: &str,
) -> PathBuf {
    let package_path = package_name
        .split('/')
        .fold(PathBuf::new(), |path, segment| path.join(segment));
    root.join(registry_host).join(package_path).join(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_host_sanitizes_authority() {
        assert_eq!(
            registry_host("https://registry.npmjs.org"),
            "registry.npmjs.org"
        );
        assert_eq!(
            registry_host("https://npm.corp.local:4873/path"),
            "npm.corp.local_4873"
        );
    }

    #[test]
    fn entry_dir_nests_scoped_packages() {
        let root = Path::new("/c");
        assert_eq!(
            entry_dir(root, "registry.npmjs.org", "@scope/name", "1.2.3"),
            PathBuf::from("/c/registry.npmjs.org/@scope/name/1.2.3")
        );
    }
}
