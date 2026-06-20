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
//! Cache failures never change match correctness: when SQLite cannot serve a
//! fingerprint, the matcher computes it from source. Failures are still counted
//! in [`FingerprintCacheStats`] so performance regressions are visible.

use std::collections::BTreeSet;
use std::path::Path;

use reverts_ir::hash::fnv1a_hex as stable_hash;
use reverts_package_matcher::{PackageSource, SourceFingerprint, fingerprint_source};
use rusqlite::{Connection, OptionalExtension, params};

use crate::errors::MatchPackagesError;
use crate::pkg_sources::cache::cache_root;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct FingerprintCacheStats {
    pub(crate) already_attached: usize,
    pub(crate) cache_hits: usize,
    pub(crate) cache_misses: usize,
    pub(crate) cache_open_errors: usize,
    pub(crate) cache_read_errors: usize,
    pub(crate) cache_write_errors: usize,
    pub(crate) corrupt_entries: usize,
    pub(crate) computed: usize,
    pub(crate) compute_errors: usize,
}

impl FingerprintCacheStats {
    pub(crate) fn total_errors(&self) -> usize {
        self.cache_open_errors
            + self.cache_read_errors
            + self.cache_write_errors
            + self.corrupt_entries
            + self.compute_errors
    }
}

fn serialize_fingerprint(fingerprint: &SourceFingerprint) -> String {
    serde_json::json!({
        "h": fingerprint.normalized_source_hash,
        "hs": fingerprint.normalized_source_hashes,
        "fs": fingerprint.function_signature_hashes,
        "td": fingerprint.top_level_declaration_hashes,
        "ies": fingerprint.import_export_surface_hashes,
        "cm": fingerprint.class_member_hashes,
        "sw": fingerprint.statement_window_hashes,
        "bb": fingerprint.block_branch_hashes,
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
        top_level_declaration_hashes: string_set("td"),
        import_export_surface_hashes: string_set("ies"),
        class_member_hashes: string_set("cm"),
        statement_window_hashes: string_set("sw"),
        block_branch_hashes: string_set("bb"),
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

    fn get(&self, content_hash: &str) -> CacheLookup {
        match self
            .connection
            .query_row(
                "SELECT fingerprint_json FROM fingerprint_cache WHERE content_hash = ?1",
                params![content_hash],
                |row| row.get::<_, String>(0),
            )
            .optional()
        {
            Ok(Some(json)) => match deserialize_fingerprint(json.as_str()) {
                Some(fingerprint) => CacheLookup::Hit(Box::new(fingerprint)),
                None => CacheLookup::Corrupt,
            },
            Ok(None) => CacheLookup::Miss,
            Err(_) => CacheLookup::ReadError,
        }
    }

    fn put(&self, content_hash: &str, fingerprint: &SourceFingerprint) -> rusqlite::Result<()> {
        // Immutable per content hash, so first writer wins; ignore conflicts and
        // any transient write error at the call site (the fingerprint is still
        // attached in-memory).
        self.connection.execute(
            "INSERT OR IGNORE INTO fingerprint_cache (content_hash, fingerprint_json)
             VALUES (?1, ?2)",
            params![content_hash, serialize_fingerprint(fingerprint)],
        )?;
        Ok(())
    }
}

enum CacheLookup {
    Hit(Box<SourceFingerprint>),
    Miss,
    Corrupt,
    ReadError,
}

/// Attach globally-cached fingerprints to `sources`, computing + caching any
/// that are missing. Keyed by source content hash so the result is shared
/// across every project. Cache failures are recorded in the returned stats and
/// leave sources to fingerprint lazily in the matcher, with identical results.
pub(crate) fn attach_global_fingerprints(sources: &mut [PackageSource]) -> FingerprintCacheStats {
    let Ok(root) = cache_root() else {
        return FingerprintCacheStats {
            cache_open_errors: 1,
            ..FingerprintCacheStats::default()
        };
    };
    attach_global_fingerprints_at_root(sources, root.as_path())
}

fn attach_global_fingerprints_at_root(
    sources: &mut [PackageSource],
    root: &Path,
) -> FingerprintCacheStats {
    let mut stats = FingerprintCacheStats::default();
    let cache = match GlobalFingerprintCache::open_at(root) {
        Ok(cache) => cache,
        Err(_) => {
            stats.cache_open_errors += 1;
            return stats;
        }
    };
    for source in sources.iter_mut() {
        if source.fingerprint.is_some() {
            stats.already_attached += 1;
            continue;
        }
        // Key by BOTH source_path and content: `fingerprint_source` parses and
        // normalizes using the path, so the same bytes under a different path
        // can fingerprint differently. The path is `name@version/rel_path`, so
        // identical package files still share a key across projects.
        let key = fingerprint_cache_key(source.source_path.as_str(), source.source.as_str());
        match cache.get(key.as_str()) {
            CacheLookup::Hit(fingerprint) => {
                stats.cache_hits += 1;
                source.fingerprint = Some(*fingerprint);
                continue;
            }
            CacheLookup::Miss => stats.cache_misses += 1,
            CacheLookup::Corrupt => {
                stats.corrupt_entries += 1;
                stats.cache_misses += 1;
            }
            CacheLookup::ReadError => {
                stats.cache_read_errors += 1;
                stats.cache_misses += 1;
            }
        }
        match fingerprint_source(source.source_path.as_str(), source.source.as_str()) {
            Ok(fingerprint) => {
                if cache.put(key.as_str(), &fingerprint).is_err() {
                    stats.cache_write_errors += 1;
                }
                stats.computed += 1;
                source.fingerprint = Some(fingerprint);
            }
            Err(_) => stats.compute_errors += 1,
        }
    }
    stats
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
            top_level_declaration_hashes: BTreeSet::from([format!("{tag}-decl")]),
            import_export_surface_hashes: BTreeSet::from([format!("{tag}-surface")]),
            class_member_hashes: BTreeSet::from([format!("{tag}-member")]),
            statement_window_hashes: BTreeSet::from([format!("{tag}-window")]),
            block_branch_hashes: BTreeSet::from([format!("{tag}-block")]),
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
        assert!(matches!(cache.get("missing"), CacheLookup::Miss));
        cache.put("h1", &sample("deadbeef")).expect("put");
        match cache.get("h1") {
            CacheLookup::Hit(fingerprint) => assert_eq!(*fingerprint, sample("deadbeef")),
            _ => panic!("expected cached fingerprint hit"),
        }
    }

    #[test]
    fn entries_persist_across_reopen_global_to_the_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let cache = GlobalFingerprintCache::open_at(dir.path()).expect("open");
            cache.put("shared", &sample("v")).expect("put");
        }
        // A different process / project pointing at the same cache dir sees it.
        let reopened = GlobalFingerprintCache::open_at(dir.path()).expect("reopen");
        match reopened.get("shared") {
            CacheLookup::Hit(fingerprint) => assert_eq!(*fingerprint, sample("v")),
            _ => panic!("expected cached fingerprint hit after reopen"),
        }
        assert!(dir.path().join("fingerprints.sqlite").is_file());
    }

    #[test]
    fn attach_global_fingerprints_reports_misses_and_hits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg",
            "pkg@1.0.0/index.js",
            "export const value = 1;",
        );
        let mut cold = vec![source.clone()];
        let cold_stats = attach_global_fingerprints_at_root(&mut cold, dir.path());
        assert_eq!(cold_stats.cache_misses, 1);
        assert_eq!(cold_stats.computed, 1);
        assert_eq!(cold_stats.cache_hits, 0);
        assert!(cold[0].fingerprint.is_some());

        let mut warm = vec![source];
        let warm_stats = attach_global_fingerprints_at_root(&mut warm, dir.path());
        assert_eq!(warm_stats.cache_hits, 1);
        assert_eq!(warm_stats.cache_misses, 0);
        assert_eq!(warm_stats.computed, 0);
        assert!(warm[0].fingerprint.is_some());
    }
}
