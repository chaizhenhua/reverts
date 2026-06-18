//! Fixed on-disk package source cache at `~/.reverts/package-cache`
//! (override: `REVERTS_PACKAGE_CACHE_DIR`). Entries are immutable, keyed by
//! `<registry-host>/<pkg>/<version>/`, holding the verified `.tgz`, a
//! `meta.json`, and the extracted `package/` tree.

use std::fs;
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use tar::Archive;

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
}
