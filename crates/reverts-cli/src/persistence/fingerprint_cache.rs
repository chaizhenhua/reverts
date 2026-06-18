//! Global, cross-project cache of package-source structural fingerprints.
//!
//! Stored in a dedicated SQLite database alongside the on-disk package cache
//! (`<cache_root>/fingerprints.sqlite`). A fingerprint is a pure, deterministic
//! function of a source's content, so entries are keyed by content hash and
//! shared by every project on the machine — each unique package source is
//! parsed and fingerprinted at most once, ever. This makes warm runs (including
//! brand-new projects that reuse already-cached package versions) skip the
//! expensive OXC parse + normalize + signature extraction.
//!
//! All operations are best-effort: if the cache cannot be opened the matcher
//! simply computes fingerprints on demand, so correctness is never affected.

use std::collections::BTreeSet;
use std::path::Path;

use reverts_ir::hash::fnv1a_hex as stable_hash;
use reverts_package_matcher::{PackageSource, SourceFingerprint, fingerprint_source};
use rusqlite::{Connection, OptionalExtension, params};

use crate::errors::MatchPackagesError;
use crate::pkg_sources::cache::cache_root;

fn serialize_fingerprint(fingerprint: &SourceFingerprint) -> String {
    serde_json::json!({
        "h": fingerprint.normalized_source_hash,
        "hs": fingerprint.normalized_source_hashes,
        "fs": fingerprint.function_signature_hashes,
        "sa": fingerprint.string_anchors,
    })
    .to_string()
}

fn deserialize_fingerprint(json: &str) -> Option<SourceFingerprint> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let string_set = |key: &str| -> BTreeSet<String> {
        value
            .get(key)
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    Some(SourceFingerprint {
        normalized_source_hash: value.get("h")?.as_str()?.to_string(),
        normalized_source_hashes: string_set("hs"),
        function_signature_hashes: string_set("fs"),
        string_anchors: string_set("sa"),
    })
}

struct GlobalFingerprintCache {
    connection: Connection,
}

impl GlobalFingerprintCache {
    fn open_at(root: &Path) -> Result<Self, MatchPackagesError> {
        std::fs::create_dir_all(root).map_err(|source| MatchPackagesError::ResolveCacheDir {
            message: format!("create cache dir {}: {source}", root.display()),
        })?;
        let connection = Connection::open(root.join("fingerprints.sqlite"))
            .map_err(MatchPackagesError::WritePackageSourceCache)?;
        // WAL lets concurrent match-packages processes read while one writes.
        let _ = connection.pragma_update(None, "journal_mode", "WAL");
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS fingerprint_cache (
                    content_hash TEXT PRIMARY KEY,
                    fingerprint_json TEXT NOT NULL
                 );",
            )
            .map_err(MatchPackagesError::WritePackageSourceCache)?;
        Ok(Self { connection })
    }

    fn get(&self, content_hash: &str) -> Option<SourceFingerprint> {
        self.connection
            .query_row(
                "SELECT fingerprint_json FROM fingerprint_cache WHERE content_hash = ?1",
                params![content_hash],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .and_then(|json| deserialize_fingerprint(json.as_str()))
    }

    fn put(&self, content_hash: &str, fingerprint: &SourceFingerprint) {
        // Immutable per content hash, so first writer wins; ignore conflicts and
        // any transient write error (the fingerprint is still attached in-memory).
        let _ = self.connection.execute(
            "INSERT OR IGNORE INTO fingerprint_cache (content_hash, fingerprint_json)
             VALUES (?1, ?2)",
            params![content_hash, serialize_fingerprint(fingerprint)],
        );
    }
}

/// Attach globally-cached fingerprints to `sources`, computing + caching any
/// that are missing. Keyed by source content hash so the result is shared
/// across every project. Best-effort: a cache-open failure leaves sources to
/// fingerprint lazily in the matcher, with identical results.
pub(crate) fn attach_global_fingerprints(sources: &mut [PackageSource]) {
    let Ok(root) = cache_root() else {
        return;
    };
    let Ok(cache) = GlobalFingerprintCache::open_at(&root) else {
        return;
    };
    for source in sources.iter_mut() {
        if source.fingerprint.is_some() {
            continue;
        }
        // Key by BOTH source_path and content: `fingerprint_source` parses and
        // normalizes using the path, so the same bytes under a different path
        // can fingerprint differently. The path is `name@version/rel_path`, so
        // identical package files still share a key across projects.
        let key = fingerprint_cache_key(source.source_path.as_str(), source.source.as_str());
        if let Some(fingerprint) = cache.get(key.as_str()) {
            source.fingerprint = Some(fingerprint);
            continue;
        }
        if let Ok(fingerprint) =
            fingerprint_source(source.source_path.as_str(), source.source.as_str())
        {
            cache.put(key.as_str(), &fingerprint);
            source.fingerprint = Some(fingerprint);
        }
    }
}

/// Cache key combining the source path and body — both inputs to
/// `fingerprint_source` — so a given `name@version/rel_path` maps to one stable
/// key shared across projects, while different paths never collide.
fn fingerprint_cache_key(source_path: &str, source: &str) -> String {
    stable_hash(format!("{source_path}\u{0}{source}").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(tag: &str) -> SourceFingerprint {
        SourceFingerprint {
            normalized_source_hash: tag.to_string(),
            normalized_source_hashes: BTreeSet::from([tag.to_string(), format!("{tag}-alt")]),
            function_signature_hashes: BTreeSet::from([format!("{tag}-sig")]),
            string_anchors: BTreeSet::from([format!("{tag}-anchor")]),
        }
    }

    #[test]
    fn serialize_round_trips() {
        let original = sample("cafe");
        let json = serialize_fingerprint(&original);
        assert_eq!(deserialize_fingerprint(json.as_str()), Some(original));
    }

    #[test]
    fn put_then_get_returns_cached_fingerprint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = GlobalFingerprintCache::open_at(dir.path()).expect("open");
        assert!(cache.get("missing").is_none());
        cache.put("h1", &sample("deadbeef"));
        assert_eq!(cache.get("h1"), Some(sample("deadbeef")));
    }

    #[test]
    fn entries_persist_across_reopen_global_to_the_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let cache = GlobalFingerprintCache::open_at(dir.path()).expect("open");
            cache.put("shared", &sample("v"));
        }
        // A different process / project pointing at the same cache dir sees it.
        let reopened = GlobalFingerprintCache::open_at(dir.path()).expect("reopen");
        assert_eq!(reopened.get("shared"), Some(sample("v")));
        assert!(dir.path().join("fingerprints.sqlite").is_file());
    }
}
