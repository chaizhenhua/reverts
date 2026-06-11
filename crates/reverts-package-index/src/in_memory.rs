use std::collections::BTreeMap;

use super::{
    AxisKind, Candidate, CfgKey, CorpusStats, ExactKey, FeatureKey, PackageFingerprintIndex,
    StructuralKey,
};

#[derive(Debug, Default)]
pub struct InMemoryFingerprintIndex {
    exact: BTreeMap<ExactKey, Vec<Candidate>>,
    cfg: BTreeMap<CfgKey, Vec<Candidate>>,
    feature: BTreeMap<FeatureKey, Vec<Candidate>>,
    structural: BTreeMap<StructuralKey, Vec<Candidate>>,
    stats: CorpusStats,
}

impl InMemoryFingerprintIndex {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_exact(&mut self, key: ExactKey, candidate: Candidate) {
        *self
            .stats
            .axis_hash_frequencies
            .entry((AxisKind::Ast, key.ast_hash))
            .or_default() += 1;
        self.exact.entry(key).or_default().push(candidate);
    }

    pub fn insert_cfg(&mut self, key: CfgKey, candidate: Candidate) {
        *self
            .stats
            .axis_hash_frequencies
            .entry((AxisKind::Cfg, key.cfg_hash))
            .or_default() += 1;
        self.cfg.entry(key).or_default().push(candidate);
    }

    pub fn insert_feature(&mut self, key: FeatureKey, candidate: Candidate) {
        *self
            .stats
            .axis_hash_frequencies
            .entry((key.kind, key.hash))
            .or_default() += 1;
        self.feature.entry(key).or_default().push(candidate);
    }

    pub fn insert_structural(&mut self, key: StructuralKey, candidate: Candidate) {
        *self
            .stats
            .axis_hash_frequencies
            .entry((AxisKind::StructuralAnchor, key.structural_anchor))
            .or_default() += 1;
        self.structural.entry(key).or_default().push(candidate);
    }
}

impl PackageFingerprintIndex for InMemoryFingerprintIndex {
    fn query_exact(&self, key: ExactKey) -> Vec<Candidate> {
        self.exact.get(&key).cloned().unwrap_or_default()
    }
    fn query_cfg(&self, key: CfgKey) -> Vec<Candidate> {
        self.cfg.get(&key).cloned().unwrap_or_default()
    }
    fn query_feature(&self, key: FeatureKey) -> Vec<Candidate> {
        self.feature.get(&key).cloned().unwrap_or_default()
    }
    fn query_structural(&self, key: StructuralKey) -> Vec<Candidate> {
        self.structural.get(&key).cloned().unwrap_or_default()
    }
    fn corpus_stats(&self) -> &CorpusStats {
        &self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PackageId;

    fn sample_candidate() -> Candidate {
        Candidate {
            package: PackageId {
                name: "pkg".into(),
                version: "1.0".into(),
            },
            variant_path: "index.js".into(),
            external_function_id: 1,
            matched_axis: AxisKind::Ast,
            matched_alternate: None,
        }
    }

    #[test]
    fn in_memory_index_inserts_and_queries_by_exact_key() {
        let mut idx = InMemoryFingerprintIndex::new();
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
        let mut idx = InMemoryFingerprintIndex::new();
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
        let idx = InMemoryFingerprintIndex::new();
        assert_eq!(idx.corpus_stats().frequency(AxisKind::Ast, 999), 1);
    }
}
