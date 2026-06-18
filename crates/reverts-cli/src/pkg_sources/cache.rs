//! Fixed on-disk package source cache at `~/.reverts/package-cache`
//! (override: `REVERTS_PACKAGE_CACHE_DIR`). Entries are immutable, keyed by
//! `<registry-host>/<pkg>/<version>/`, holding the verified `.tgz`, a
//! `meta.json`, and the extracted `package/` tree.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::read::GzDecoder;
use tar::Archive;

use crate::errors::MatchPackagesError;
use crate::pkg_sources::registry::{self, PackumentVersion};

/// Resolve the cache root: `REVERTS_PACKAGE_CACHE_DIR` if set, else
/// `$HOME/.reverts/package-cache`.
pub(crate) fn cache_root() -> Result<PathBuf, MatchPackagesError> {
    if let Ok(dir) = std::env::var("REVERTS_PACKAGE_CACHE_DIR")
        && !dir.trim().is_empty()
    {
        return Ok(PathBuf::from(dir));
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

/// Extract a gzipped tar (npm tarball) into `dest`, creating it. npm tarballs
/// root every entry under `package/`, so after extraction `dest/package/`
/// holds the package. Path traversal entries (`..`) are rejected.
pub(crate) fn extract_tarball_gz(
    package_name: &str,
    package_version: &str,
    tarball: &[u8],
    dest: &Path,
) -> Result<(), MatchPackagesError> {
    let make_err = |message: String| MatchPackagesError::ExtractPackageSource {
        package_name: package_name.to_string(),
        package_version: package_version.to_string(),
        message,
    };
    fs::create_dir_all(dest).map_err(|source| make_err(format!("create dest: {source}")))?;
    let mut archive = Archive::new(GzDecoder::new(tarball));
    let entries = archive
        .entries()
        .map_err(|source| make_err(format!("read tar entries: {source}")))?;
    for entry in entries {
        let mut entry = entry.map_err(|source| make_err(format!("read tar entry: {source}")))?;
        let path = entry
            .path()
            .map_err(|source| make_err(format!("entry path: {source}")))?
            .into_owned();
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(make_err(format!(
                "rejected path traversal entry: {}",
                path.display()
            )));
        }
        entry
            .unpack_in(dest)
            .map_err(|source| make_err(format!("unpack {}: {source}", path.display())))?;
    }
    Ok(())
}

const META_FILE: &str = "meta.json";
const TARBALL_FILE: &str = "package.tgz";
const PACKAGE_DIR: &str = "package";

fn write_meta(
    entry: &Path,
    package_name: &str,
    version: &str,
    integrity: &str,
    tarball_url: &str,
) -> Result<(), MatchPackagesError> {
    let fetched_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|source| MatchPackagesError::ExtractPackageSource {
            package_name: package_name.to_string(),
            package_version: version.to_string(),
            message: format!("system clock is before the Unix epoch: {source}"),
        })?;
    let meta = serde_json::json!({
        "name": package_name,
        "version": version,
        "integrity": integrity,
        "tarball_url": tarball_url,
        "fetched_at": fetched_at,
    });
    let body = serde_json::to_vec_pretty(&meta).map_err(|source| {
        MatchPackagesError::ExtractPackageSource {
            package_name: package_name.to_string(),
            package_version: version.to_string(),
            message: format!("serialize meta: {source}"),
        }
    })?;
    fs::write(entry.join(META_FILE), body).map_err(|source| {
        MatchPackagesError::ExtractPackageSource {
            package_name: package_name.to_string(),
            package_version: version.to_string(),
            message: format!("write meta: {source}"),
        }
    })
}

fn meta_integrity_matches(entry: &Path, integrity: &str) -> bool {
    let Ok(body) = fs::read(entry.join(META_FILE)) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return false;
    };
    value.get("integrity").and_then(serde_json::Value::as_str) == Some(integrity)
}

/// Ensure `<root>/<host>/<pkg>/<version>/package/` exists and is integrity-clean,
/// returning the path to that `package/` directory. `download` fetches tarball
/// bytes for a URL (injected for testing).
pub(crate) fn ensure_package_source(
    root: &Path,
    registry_host: &str,
    package_name: &str,
    version: &str,
    dist: &PackumentVersion,
    download: impl Fn(&str) -> Result<Vec<u8>, MatchPackagesError>,
) -> Result<PathBuf, MatchPackagesError> {
    let make_err = |message: String| MatchPackagesError::ExtractPackageSource {
        package_name: package_name.to_string(),
        package_version: version.to_string(),
        message,
    };
    let integrity =
        dist.integrity
            .as_deref()
            .ok_or_else(|| MatchPackagesError::PackageSourceIntegrity {
                package_name: package_name.to_string(),
                package_version: version.to_string(),
                message: "registry provided no integrity hash".to_string(),
            })?;
    let entry = entry_dir(root, registry_host, package_name, version);
    let package_dir = entry.join(PACKAGE_DIR);

    // Case 1: extracted tree present and integrity recorded matches → hit.
    if package_dir.is_dir() && meta_integrity_matches(&entry, integrity) {
        return Ok(package_dir);
    }

    // Case 2: tarball already on disk and verifies → re-extract locally, no
    // network and no re-write of the tarball.
    let tarball_path = entry.join(TARBALL_FILE);
    if let Ok(bytes) = fs::read(&tarball_path)
        && registry::verify_integrity(package_name, version, &bytes, Some(integrity)).is_ok()
    {
        return commit_entry(
            &entry,
            package_name,
            version,
            integrity,
            &dist.tarball,
            &bytes,
        );
    }

    // Case 3: miss → download, verify, commit.
    let bytes = download(&dist.tarball)?;
    registry::verify_integrity(package_name, version, &bytes, Some(integrity))?;
    fs::create_dir_all(&entry).map_err(|source| make_err(format!("create entry dir: {source}")))?;
    fs::write(&tarball_path, &bytes)
        .map_err(|source| make_err(format!("write tarball: {source}")))?;
    commit_entry(
        &entry,
        package_name,
        version,
        integrity,
        &dist.tarball,
        &bytes,
    )
}

/// Extract into a temp sibling dir, atomically swap into `package/`, then write
/// `meta.json` last as the commit marker.
fn commit_entry(
    entry: &Path,
    package_name: &str,
    version: &str,
    integrity: &str,
    tarball_url: &str,
    tarball: &[u8],
) -> Result<PathBuf, MatchPackagesError> {
    let make_err = |message: String| MatchPackagesError::ExtractPackageSource {
        package_name: package_name.to_string(),
        package_version: version.to_string(),
        message,
    };
    fs::create_dir_all(entry).map_err(|source| make_err(format!("create entry dir: {source}")))?;
    let staging = entry.join(format!(".staging-{}", std::process::id()));
    if staging.exists() {
        fs::remove_dir_all(&staging)
            .map_err(|source| make_err(format!("clear staging: {source}")))?;
    }
    extract_tarball_gz(package_name, version, tarball, &staging)?;
    let extracted_package = staging.join(PACKAGE_DIR);
    if !extracted_package.is_dir() {
        return Err(make_err(
            "tarball did not contain a package/ root".to_string(),
        ));
    }
    let final_package = entry.join(PACKAGE_DIR);
    if final_package.exists() {
        fs::remove_dir_all(&final_package)
            .map_err(|source| make_err(format!("clear old package: {source}")))?;
    }
    fs::rename(&extracted_package, &final_package)
        .map_err(|source| make_err(format!("commit package dir: {source}")))?;
    let _ = fs::remove_dir_all(&staging);
    write_meta(entry, package_name, version, integrity, tarball_url)?;
    Ok(final_package)
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

    #[test]
    fn extract_tarball_writes_package_tree() {
        // Build a tiny gzipped tar in-memory: package/package.json + package/index.js
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let files: [(&str, &str); 2] = [
                ("package/package.json", r#"{"name":"x","version":"1.0.0"}"#),
                ("package/index.js", "module.exports = 1;\n"),
            ];
            for (name, body) in files {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, name, body.as_bytes())
                    .expect("append");
            }
            builder.finish().expect("finish tar");
        }
        let mut gz = Vec::new();
        {
            use std::io::Write as _;
            let mut encoder =
                flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
            encoder.write_all(&tar_bytes).expect("gz write");
            encoder.finish().expect("gz finish");
        }
        let dir = tempfile::tempdir().expect("tempdir");
        extract_tarball_gz("x", "1.0.0", &gz, dir.path()).expect("extract");
        let pkg_json = dir.path().join("package").join("package.json");
        assert!(pkg_json.is_file());
        assert!(dir.path().join("package").join("index.js").is_file());
    }

    #[test]
    fn extract_tarball_rejects_traversal_entry() {
        // Build a gzipped tar whose single entry escapes via "..".
        // We craft raw POSIX tar bytes because tar::Builder validates paths and
        // would reject "package/../evil.txt" itself — but a real attacker's
        // archive won't use our Builder, so we need to test the reader guard
        // against a genuinely malformed archive.
        //
        // POSIX tar header layout (512 bytes):
        //   [0..100]  name (NUL-terminated)
        //   [100..108] mode (octal string)
        //   [108..116] uid
        //   [116..124] gid
        //   [124..136] size (octal string)
        //   [136..148] mtime (octal string)
        //   [148..156] checksum
        //   [156]      typeflag ('0' = regular file)
        //   [157..265] linkname
        //   [265..500] (padding / ustar extension)
        //   then: file data padded to 512-byte boundary
        //   then: two 512-byte zero blocks (end-of-archive)
        let traversal_path = b"package/../evil.txt";
        let body = b"pwned";

        let mut header = [0u8; 512];
        header[..traversal_path.len()].copy_from_slice(traversal_path);
        // mode: "0000644\0"
        header[100..108].copy_from_slice(b"0000644\0");
        // uid / gid: all zeros is fine for a test
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        // size: "0000005\0" (5 bytes)
        header[124..136].copy_from_slice(b"00000000005\0");
        // mtime: "00000000000\0"
        header[136..148].copy_from_slice(b"00000000000\0");
        // typeflag: '0' regular file
        header[156] = b'0';
        // checksum placeholder: fill with spaces first, then compute
        header[148..156].copy_from_slice(b"        ");
        let cksum: u32 = header.iter().map(|&b| b as u32).sum();
        // write checksum as 6-digit octal + NUL + space
        let cksum_str = format!("{:06o}\0 ", cksum);
        header[148..156].copy_from_slice(cksum_str.as_bytes());

        // data block (512 bytes, body + padding)
        let mut data_block = [0u8; 512];
        data_block[..body.len()].copy_from_slice(body);

        // two end-of-archive zero blocks
        let eof_blocks = [0u8; 1024];

        let mut tar_bytes: Vec<u8> = Vec::new();
        tar_bytes.extend_from_slice(&header);
        tar_bytes.extend_from_slice(&data_block);
        tar_bytes.extend_from_slice(&eof_blocks);

        let mut gz = Vec::new();
        {
            use std::io::Write as _;
            let mut encoder =
                flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
            encoder.write_all(&tar_bytes).expect("gz write");
            encoder.finish().expect("gz finish");
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let result = extract_tarball_gz("x", "1.0.0", &gz, dir.path());
        assert!(
            matches!(result, Err(MatchPackagesError::ExtractPackageSource { .. })),
            "expected ExtractPackageSource error for path-traversal entry, got: {:?}",
            result
        );
        // Nothing should have escaped the destination.
        assert!(
            !dir.path()
                .parent()
                .expect("parent")
                .join("evil.txt")
                .exists()
        );
    }

    fn sample_tarball_gz() -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let body = r#"{"name":"x","version":"1.0.0"}"#;
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "package/package.json", body.as_bytes())
                .expect("append");
            builder.finish().expect("finish");
        }
        let mut gz = Vec::new();
        {
            use std::io::Write as _;
            let mut encoder =
                flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
            encoder.write_all(&tar_bytes).expect("gz");
            encoder.finish().expect("gz finish");
        }
        gz
    }

    fn dist_for(gz: &[u8]) -> PackumentVersion {
        use base64::Engine as _;
        use sha2::{Digest, Sha512};
        let integrity = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(Sha512::digest(gz))
        );
        PackumentVersion {
            tarball: "https://r/x-1.0.0.tgz".to_string(),
            integrity: Some(integrity),
        }
    }

    #[test]
    fn ensure_downloads_then_hits_cache() {
        let gz = sample_tarball_gz();
        let dist = dist_for(&gz);
        let root = tempfile::tempdir().expect("tempdir");
        let calls = std::cell::Cell::new(0u32);
        let download = |_url: &str| {
            calls.set(calls.get() + 1);
            Ok(gz.clone())
        };
        // Miss → downloads once.
        let pkg = ensure_package_source(
            root.path(),
            "registry.npmjs.org",
            "x",
            "1.0.0",
            &dist,
            download,
        )
        .expect("first ensure");
        assert!(pkg.join("package.json").is_file());
        assert_eq!(calls.get(), 1);
        // Hit → no further download.
        let pkg2 = ensure_package_source(
            root.path(),
            "registry.npmjs.org",
            "x",
            "1.0.0",
            &dist,
            download,
        )
        .expect("second ensure");
        assert_eq!(pkg, pkg2);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn ensure_rejects_corrupt_download() {
        let gz = sample_tarball_gz();
        let dist = dist_for(&gz);
        let root = tempfile::tempdir().expect("tempdir");
        let download = |_url: &str| Ok(b"not the real tarball".to_vec());
        let err = ensure_package_source(
            root.path(),
            "registry.npmjs.org",
            "x",
            "1.0.0",
            &dist,
            download,
        )
        .expect_err("integrity must fail");
        assert!(matches!(
            err,
            MatchPackagesError::PackageSourceIntegrity { .. }
        ));
        assert!(
            !root
                .path()
                .join("registry.npmjs.org/x/1.0.0/package")
                .exists()
        );
    }

    #[test]
    fn ensure_reextracts_from_tarball_without_download() {
        let gz = sample_tarball_gz();
        let dist = dist_for(&gz);
        let root = tempfile::tempdir().expect("tempdir");
        let calls = std::cell::Cell::new(0u32);
        let download = |_url: &str| {
            calls.set(calls.get() + 1);
            Ok(gz.clone())
        };
        // Miss → downloads once and commits.
        let pkg = ensure_package_source(
            root.path(),
            "registry.npmjs.org",
            "x",
            "1.0.0",
            &dist,
            download,
        )
        .expect("first ensure");
        assert_eq!(calls.get(), 1);
        // Simulate an interrupted prior commit: drop meta.json and the package dir,
        // leaving only the verified package.tgz on disk.
        let entry = pkg.parent().expect("entry dir").to_path_buf();
        std::fs::remove_file(entry.join("meta.json")).expect("rm meta");
        std::fs::remove_dir_all(&pkg).expect("rm package dir");
        // Case 2: re-extract from the cached tarball, no new download.
        let pkg2 = ensure_package_source(
            root.path(),
            "registry.npmjs.org",
            "x",
            "1.0.0",
            &dist,
            download,
        )
        .expect("re-extract");
        assert!(pkg2.join("package.json").is_file());
        assert_eq!(calls.get(), 1);
    }
}
