use std::collections::BTreeMap;

use super::{
    AxisKind, Candidate, CfgKey, CorpusStats, ExactKey, FeatureKey, PackageOwner, StructuralKey,
};

#[derive(Debug, Clone)]
pub struct FingerprintIndex<Owner> {
    exact: BTreeMap<ExactKey, Vec<Candidate<Owner>>>,
    cfg: BTreeMap<CfgKey, Vec<Candidate<Owner>>>,
    feature: BTreeMap<FeatureKey, Vec<Candidate<Owner>>>,
    structural: BTreeMap<StructuralKey, Vec<Candidate<Owner>>>,
    stats: CorpusStats,
}

/// Convenience alias matching the package-matcher's historical signature.
pub type PackageFingerprintIndex = FingerprintIndex<PackageOwner>;

impl<Owner> Default for FingerprintIndex<Owner> {
    fn default() -> Self {
        Self {
            exact: BTreeMap::new(),
            cfg: BTreeMap::new(),
            feature: BTreeMap::new(),
            structural: BTreeMap::new(),
            stats: CorpusStats::default(),
        }
    }
}

impl<Owner: Clone> FingerprintIndex<Owner> {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_exact(&mut self, key: ExactKey, candidate: Candidate<Owner>) {
        *self
            .stats
            .axis_hash_frequencies
            .entry((AxisKind::Ast, key.ast_hash))
            .or_default() += 1;
        self.exact.entry(key).or_default().push(candidate);
    }

    pub fn insert_cfg(&mut self, key: CfgKey, candidate: Candidate<Owner>) {
        *self
            .stats
            .axis_hash_frequencies
            .entry((AxisKind::Cfg, key.cfg_hash))
            .or_default() += 1;
        self.cfg.entry(key).or_default().push(candidate);
    }

    pub fn insert_feature(&mut self, key: FeatureKey, candidate: Candidate<Owner>) {
        *self
            .stats
            .axis_hash_frequencies
            .entry((key.kind, key.hash))
            .or_default() += 1;
        self.feature.entry(key).or_default().push(candidate);
    }

    pub fn insert_structural(&mut self, key: StructuralKey, candidate: Candidate<Owner>) {
        *self
            .stats
            .axis_hash_frequencies
            .entry((AxisKind::StructuralAnchor, key.structural_anchor))
            .or_default() += 1;
        self.structural.entry(key).or_default().push(candidate);
    }

    #[must_use]
    pub fn query_exact(&self, key: ExactKey) -> Vec<Candidate<Owner>> {
        self.exact.get(&key).cloned().unwrap_or_default()
    }

    #[must_use]
    pub fn query_cfg(&self, key: CfgKey) -> Vec<Candidate<Owner>> {
        self.cfg.get(&key).cloned().unwrap_or_default()
    }

    #[must_use]
    pub fn query_feature(&self, key: FeatureKey) -> Vec<Candidate<Owner>> {
        self.feature.get(&key).cloned().unwrap_or_default()
    }

    #[must_use]
    pub fn query_structural(&self, key: StructuralKey) -> Vec<Candidate<Owner>> {
        self.structural.get(&key).cloned().unwrap_or_default()
    }
}

/// Borrowing query variants for hot scoring loops that only need to read
/// candidate identities. Avoids the per-query `Vec<Candidate>` allocation
/// the cloning API forces — significant when a tier scorer fires ~10
/// queries per fingerprint on a corpus with tens of thousands of
/// fingerprints.
impl<Owner> FingerprintIndex<Owner> {
    #[must_use]
    pub fn lookup_exact(&self, key: ExactKey) -> &[Candidate<Owner>] {
        self.exact.get(&key).map(Vec::as_slice).unwrap_or(&[])
    }

    #[must_use]
    pub fn lookup_cfg(&self, key: CfgKey) -> &[Candidate<Owner>] {
        self.cfg.get(&key).map(Vec::as_slice).unwrap_or(&[])
    }

    #[must_use]
    pub fn lookup_feature(&self, key: FeatureKey) -> &[Candidate<Owner>] {
        self.feature.get(&key).map(Vec::as_slice).unwrap_or(&[])
    }

    #[must_use]
    pub fn lookup_structural(&self, key: StructuralKey) -> &[Candidate<Owner>] {
        self.structural.get(&key).map(Vec::as_slice).unwrap_or(&[])
    }
}

impl<Owner> FingerprintIndex<Owner> {
    #[must_use]
    pub fn corpus_stats(&self) -> &CorpusStats {
        &self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PackageCandidate, PackageId};

    fn sample_candidate() -> PackageCandidate {
        PackageCandidate {
            owner: PackageOwner {
                package: PackageId {
                    name: "pkg".into(),
                    version: "1.0".into(),
                },
                variant_path: "index.js".into(),
                external_importable: true,
            },
            external_function_id: 1,
            matched_axis: AxisKind::Ast,
            matched_alternate: None,
        }
    }

    #[test]
    fn in_memory_index_inserts_and_queries_by_exact_key() {
        let mut idx = PackageFingerprintIndex::new();
        let key = ExactKey {
            param_count: 2,
            statement_count: 3,
            ast_hash: 42,
        };
        idx.insert_exact(key, sample_candidate());

        let candidates = idx.query_exact(key);
        assert_eq!(candidates.len(), 1);

        let miss = idx.query_exact(ExactKey {
            param_count: 2,
            statement_count: 3,
            ast_hash: 99,
        });
        assert!(miss.is_empty());
    }

    #[test]
    fn in_memory_index_tracks_corpus_frequency() {
        let mut idx = PackageFingerprintIndex::new();
        let key = ExactKey {
            param_count: 2,
            statement_count: 3,
            ast_hash: 42,
        };
        idx.insert_exact(key, sample_candidate());
        idx.insert_exact(key, sample_candidate());

        assert_eq!(idx.corpus_stats().frequency(AxisKind::Ast, 42), 2);
    }

    #[test]
    fn in_memory_index_returns_default_frequency_for_unseen_hash() {
        let idx = PackageFingerprintIndex::new();
        assert_eq!(idx.corpus_stats().frequency(AxisKind::Ast, 999), 1);
    }
}
