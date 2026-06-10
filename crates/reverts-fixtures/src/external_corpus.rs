use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// One entry from the external bundler corpus on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCase {
    pub root_dir: PathBuf,
    pub manifest: CaseManifest,
}

impl ExternalCase {
    /// Absolute path to the artifact entry file declared by the case manifest.
    #[must_use]
    pub fn artifact_entry_path(&self) -> PathBuf {
        self.root_dir.join(&self.manifest.artifact.entry)
    }

    /// Load the artifact entry source as a UTF-8 string.
    pub fn read_artifact_entry(&self) -> Result<String, io::Error> {
        fs::read_to_string(self.artifact_entry_path())
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct CaseManifest {
    pub id: String,
    pub category: String,
    pub artifact: CaseArtifact,
    pub expectations: CaseExpectations,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct CaseArtifact {
    pub tool: String,
    pub version: String,
    pub entry: String,
    #[serde(default)]
    pub outputs: Vec<String>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub module_kind: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct CaseExpectations {
    pub bundler_family: String,
    #[serde(default)]
    pub wrappers: Vec<serde_json::Value>,
    #[serde(default)]
    pub helpers: Vec<serde_json::Value>,
    #[serde(default)]
    pub must_recover_modules: Vec<serde_json::Value>,
    #[serde(default)]
    pub must_recover_exports: Vec<serde_json::Value>,
    #[serde(default)]
    pub verification_levels: Vec<String>,
}

/// Workspace-relative path to the corpus root, resolved from the
/// `reverts-fixtures` crate manifest dir.
#[must_use]
pub fn external_corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/external/cases")
        .canonicalize()
        .expect("external corpus root must exist; run from a checkout that includes fixtures")
}

/// Load every case under the corpus root. Cases are returned in deterministic
/// (lexicographic) order by `(bundler, category, case-id)`.
pub fn load_external_cases() -> Result<Vec<ExternalCase>, CorpusError> {
    let root = external_corpus_root();
    let mut bundlers = sorted_subdirs(&root)?;
    bundlers.retain(|path| path.is_dir());

    let mut cases = Vec::new();
    for bundler_dir in bundlers {
        let categories = sorted_subdirs(&bundler_dir)?;
        for category_dir in categories {
            if !category_dir.is_dir() {
                continue;
            }
            for case_dir in sorted_subdirs(&category_dir)? {
                if !case_dir.is_dir() {
                    continue;
                }
                let manifest_path = case_dir.join("case.json");
                if !manifest_path.exists() {
                    continue;
                }
                let raw = fs::read_to_string(&manifest_path).map_err(|source| {
                    CorpusError::ReadManifest {
                        path: manifest_path.clone(),
                        source,
                    }
                })?;
                let manifest: CaseManifest =
                    serde_json::from_str(&raw).map_err(|source| CorpusError::ParseManifest {
                        path: manifest_path.clone(),
                        source,
                    })?;
                cases.push(ExternalCase {
                    root_dir: case_dir,
                    manifest,
                });
            }
        }
    }
    Ok(cases)
}

fn sorted_subdirs(parent: &Path) -> Result<Vec<PathBuf>, CorpusError> {
    let mut entries = fs::read_dir(parent)
        .map_err(|source| CorpusError::ReadDir {
            path: parent.to_path_buf(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| CorpusError::ReadDir {
            path: parent.to_path_buf(),
            source,
        })?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    Ok(entries.into_iter().map(|entry| entry.path()).collect())
}

#[derive(Debug)]
pub enum CorpusError {
    ReadDir {
        path: PathBuf,
        source: io::Error,
    },
    ReadManifest {
        path: PathBuf,
        source: io::Error,
    },
    ParseManifest {
        path: PathBuf,
        source: serde_json::Error,
    },
}

impl std::fmt::Display for CorpusError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadDir { path, source } => {
                write!(formatter, "read directory {}: {source}", path.display())
            }
            Self::ReadManifest { path, source } => {
                write!(formatter, "read manifest {}: {source}", path.display())
            }
            Self::ParseManifest { path, source } => {
                write!(formatter, "parse manifest {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for CorpusError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ReadDir { source, .. } | Self::ReadManifest { source, .. } => Some(source),
            Self::ParseManifest { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ExternalCase, load_external_cases};
    use std::collections::BTreeMap;

    #[test]
    fn corpus_loader_parses_every_case_manifest() {
        let cases = load_external_cases().expect("corpus should load");
        assert!(
            cases.len() >= 1000,
            "corpus must contain the curated bundler set (got {})",
            cases.len()
        );

        let mut by_family = BTreeMap::<String, usize>::new();
        for case in &cases {
            *by_family
                .entry(case.manifest.expectations.bundler_family.clone())
                .or_default() += 1;
            assert!(
                case.artifact_entry_path().exists(),
                "case {} declares missing artifact {}",
                case.manifest.id,
                case.manifest.artifact.entry,
            );
        }

        // The fixtures classify themselves into a bounded set of bundler
        // *families* (e.g. rspack reports `bundler_family = webpack` because it
        // emits webpack-compatible output). Lock in the family roster so a
        // future curation change shows up as a clear test failure.
        let families = by_family.keys().cloned().collect::<Vec<_>>();
        assert_eq!(
            families,
            vec![
                "babel".to_string(),
                "bun".to_string(),
                "esbuild".to_string(),
                "parcel".to_string(),
                "rolldown".to_string(),
                "rollup".to_string(),
                "swc".to_string(),
                "tsc".to_string(),
                "vite".to_string(),
                "webpack".to_string(),
            ],
            "unexpected bundler-family roster: {by_family:?}",
        );
    }

    #[test]
    fn corpus_cases_can_read_artifact_entry_source() {
        let cases = load_external_cases().expect("corpus should load");
        // Spot-check the first case from each bundler family — full corpus
        // I/O is exercised by the integration test in reverts-pipeline.
        let mut seen = std::collections::BTreeSet::<String>::new();
        for case in cases {
            if !seen.insert(case.manifest.expectations.bundler_family.clone()) {
                continue;
            }
            let _: String = ExternalCase::read_artifact_entry(&case).unwrap_or_else(|err| {
                panic!("case {} cannot read artifact: {err}", case.manifest.id)
            });
        }
    }
}
