use std::collections::BTreeSet;
use std::path::Path;

use reverts_input::{
    InputRows, ModuleInput, PackageAttributionInput, PackageAttributionStatus, PackageEmissionMode,
};
use reverts_ir::{ModuleId, ModuleKind, split_bare_specifier};
use reverts_js::{JsError, normalize_source_for_pipeline};
use reverts_observe::{AuditFinding, AuditReport, FindingCode};

#[derive(Debug, Clone, PartialEq, Eq)]
/// Package source candidate with a verified import surface.
pub struct PackageSource {
    /// npm package name.
    pub package_name: String,
    /// concrete package version.
    pub package_version: String,
    /// import specifier that may be emitted if the match is accepted.
    pub export_specifier: String,
    /// package source path used as the parser path hint.
    pub source_path: String,
    /// package source body.
    pub source: String,
}

impl PackageSource {
    /// Creates an external package source candidate.
    #[must_use]
    pub fn external(
        package_name: impl Into<String>,
        package_version: impl Into<String>,
        export_specifier: impl Into<String>,
        source_path: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            package_name: package_name.into(),
            package_version: package_version.into(),
            export_specifier: export_specifier.into(),
            source_path: source_path.into(),
            source: source.into(),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
/// Exact package matcher over normalized module and package sources.
pub struct ExactPackageMatcher;

impl ExactPackageMatcher {
    /// Matches package modules in unvalidated input rows before generation.
    ///
    /// The matcher reads module source only through `InputRows::module_source_slice`
    /// and normalizes both sides through `reverts-js` before exact comparison.
    #[must_use]
    pub fn match_rows(
        self,
        rows: &InputRows,
        package_sources: &[PackageSource],
    ) -> PackageMatchReport {
        let mut audit = AuditReport::default();
        let source_index = index_package_sources(package_sources, &mut audit);
        let mut matches = Vec::new();
        let mut attributions = Vec::new();

        for module in rows
            .modules
            .iter()
            .filter(|module| module.kind == ModuleKind::Package)
        {
            if has_accepted_attribution(rows, module.id) {
                continue;
            }

            let Some(slice) = rows.module_source_slice(module.id) else {
                audit.push(
                    AuditFinding::error(
                        FindingCode::MissingPackageSource,
                        "package module has no real source slice for matching",
                    )
                    .with_module(module.id.0.to_string())
                    .with_binding(module.original_name.clone()),
                );
                continue;
            };

            let normalized = match normalize_source(slice.source_file_path, slice.source) {
                Ok(normalized) => normalized,
                Err(message) => {
                    audit.push(
                        AuditFinding::error(FindingCode::UnparseablePackageSource, message)
                            .with_module(module.id.0.to_string())
                            .with_binding(module.original_name.clone()),
                    );
                    continue;
                }
            };

            let hash = stable_hash(normalized.as_bytes());
            let Some(candidates) = source_index.candidates_for_hash(hash.as_str()) else {
                continue;
            };

            match select_unique_candidate(module, candidates) {
                CandidateSelection::Selected(source) => {
                    let attribution = accepted_attribution(module.id, source);
                    matches.push(PackageMatch {
                        module_id: module.id,
                        package_name: source.package_name.clone(),
                        package_version: source.package_version.clone(),
                        export_specifier: source.export_specifier.clone(),
                        source_path: source.source_path.clone(),
                        normalized_source_hash: hash.clone(),
                    });
                    attributions.push(attribution);
                }
                CandidateSelection::Ambiguous => {
                    audit.push(
                        AuditFinding::error(
                            FindingCode::AmbiguousPackageMatch,
                            "package source matched more than one package candidate",
                        )
                        .with_module(module.id.0.to_string())
                        .with_binding(
                            module
                                .package_name
                                .clone()
                                .unwrap_or_else(|| module.original_name.clone()),
                        ),
                    );
                }
                CandidateSelection::NoCandidate => {}
            }
        }

        PackageMatchReport {
            attributions,
            matches,
            audit,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Result of a package matching pass.
pub struct PackageMatchReport {
    /// Accepted attributions that can be persisted by the caller.
    pub attributions: Vec<PackageAttributionInput>,
    /// Match evidence for accepted attributions.
    pub matches: Vec<PackageMatch>,
    /// Ambiguity, missing source, and parse findings.
    pub audit: AuditReport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Exact package match evidence.
pub struct PackageMatch {
    /// matched module id.
    pub module_id: ModuleId,
    /// matched npm package name.
    pub package_name: String,
    /// matched concrete package version.
    pub package_version: String,
    /// accepted import specifier.
    pub export_specifier: String,
    /// package source path that matched the module body.
    pub source_path: String,
    /// stable hash of the normalized matched source.
    pub normalized_source_hash: String,
}

fn has_accepted_attribution(rows: &InputRows, module_id: ModuleId) -> bool {
    rows.package_attributions.iter().any(|attribution| {
        attribution.module_id == module_id
            && attribution.status == PackageAttributionStatus::Accepted
            && attribution.emission_mode == PackageEmissionMode::ExternalImport
    })
}

fn index_package_sources<'a>(
    package_sources: &'a [PackageSource],
    audit: &mut AuditReport,
) -> PackageSourceIndex<'a> {
    let mut entries = Vec::new();
    for source in package_sources {
        match normalize_source(source.source_path.as_str(), source.source.as_str()) {
            Ok(normalized) => {
                entries.push(PackageSourceIndexEntry {
                    normalized_source_hash: stable_hash(normalized.as_bytes()),
                    source,
                });
            }
            Err(message) => {
                audit.push(
                    AuditFinding::error(FindingCode::UnparseablePackageSource, message)
                        .with_module(source.source_path.clone())
                        .with_binding(format!(
                            "{}@{}",
                            source.package_name, source.package_version
                        )),
                );
            }
        }
    }
    entries.sort_by(|left, right| {
        left.normalized_source_hash
            .cmp(&right.normalized_source_hash)
            .then_with(|| left.source.package_name.cmp(&right.source.package_name))
            .then_with(|| {
                left.source
                    .package_version
                    .cmp(&right.source.package_version)
            })
            .then_with(|| {
                left.source
                    .export_specifier
                    .cmp(&right.source.export_specifier)
            })
            .then_with(|| left.source.source_path.cmp(&right.source.source_path))
    });
    PackageSourceIndex { entries }
}

#[derive(Debug)]
struct PackageSourceIndex<'a> {
    entries: Vec<PackageSourceIndexEntry<'a>>,
}

impl<'a> PackageSourceIndex<'a> {
    fn candidates_for_hash(&self, hash: &str) -> Option<&[PackageSourceIndexEntry<'a>]> {
        let index = self
            .entries
            .binary_search_by(|entry| entry.normalized_source_hash.as_str().cmp(hash))
            .ok()?;

        let mut start = index;
        while start > 0 && self.entries[start - 1].normalized_source_hash == hash {
            start -= 1;
        }

        let mut end = index + 1;
        while end < self.entries.len() && self.entries[end].normalized_source_hash == hash {
            end += 1;
        }

        Some(&self.entries[start..end])
    }
}

#[derive(Debug)]
struct PackageSourceIndexEntry<'a> {
    normalized_source_hash: String,
    source: &'a PackageSource,
}

fn normalize_source(path: &str, source: &str) -> Result<String, String> {
    normalize_source_for_pipeline(source, Some(Path::new(path)))
        .map_err(|error| parse_error_message(&error))
}

enum CandidateSelection<'a> {
    Selected(&'a PackageSource),
    Ambiguous,
    NoCandidate,
}

fn select_unique_candidate<'a>(
    module: &ModuleInput,
    candidates: &[PackageSourceIndexEntry<'a>],
) -> CandidateSelection<'a> {
    let filtered = candidates
        .iter()
        .map(|candidate| candidate.source)
        .filter(|source| {
            module
                .package_name
                .as_deref()
                .is_none_or(|package_name| package_name == source.package_name)
        })
        .collect::<Vec<_>>();

    if filtered.is_empty() {
        return CandidateSelection::NoCandidate;
    }

    let unique_keys = filtered
        .iter()
        .map(|source| {
            (
                source.package_name.as_str(),
                source.package_version.as_str(),
                source.export_specifier.as_str(),
                source.source_path.as_str(),
            )
        })
        .collect::<BTreeSet<_>>();

    if unique_keys.len() == 1 {
        CandidateSelection::Selected(filtered[0])
    } else {
        CandidateSelection::Ambiguous
    }
}

fn accepted_attribution(module_id: ModuleId, source: &PackageSource) -> PackageAttributionInput {
    let mut attribution = PackageAttributionInput::accepted_external(
        module_id,
        source.package_name.as_str(),
        source.package_version.as_str(),
        source.export_specifier.as_str(),
    );
    if let Some((_package_name, Some(subpath))) = split_bare_specifier(&source.export_specifier) {
        attribution = attribution.with_subpath(subpath);
    }
    attribution
}

fn parse_error_message(error: &JsError) -> String {
    match error {
        JsError::ParseFailed(errors) => errors.first().map_or_else(
            || "source could not be parsed".to_string(),
            |error| {
                let diagnostic = error
                    .diagnostics
                    .first()
                    .map_or("no diagnostic", String::as_str);
                format!(
                    "source could not be parsed as {}: {diagnostic}",
                    error.source_type
                )
            },
        ),
    }
}

fn stable_hash(bytes: &[u8]) -> String {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0100_0000_01b3;

#[cfg(test)]
mod tests {
    use reverts_input::{
        InputRows, ModuleInput, PackageAttributionInput, ProjectInput, SourceFileInput, SourceSpan,
    };
    use reverts_ir::ModuleId;
    use reverts_observe::FindingCode;

    use super::{ExactPackageMatcher, PackageSource, index_package_sources};

    fn rows_with_package_source(source: &str) -> InputRows {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some(source.to_string()),
        ));
        rows.modules.push(
            ModuleInput::package(ModuleId(10), "m10", "pkg/module.ts", "pkg", None)
                .with_source_file(1),
        );
        rows
    }

    #[test]
    fn exact_match_uses_normalized_source_before_accepting_attribution() {
        let rows = rows_with_package_source("export function add(a,b){return a+b}");
        let package_sources = [PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/add",
            "add.js",
            "export function add(a, b) {\n  return a + b;\n}",
        )];

        let report = ExactPackageMatcher.match_rows(&rows, &package_sources);

        assert!(report.audit.is_clean());
        assert_eq!(report.attributions.len(), 1);
        assert_eq!(report.attributions[0].package_name, "pkg");
        assert_eq!(
            report.attributions[0].package_version.as_deref(),
            Some("1.2.3")
        );
        assert_eq!(
            report.attributions[0].export_specifier.as_deref(),
            Some("pkg/add")
        );
        assert_eq!(report.attributions[0].subpath.as_deref(), Some("add"));
    }

    #[test]
    fn ambiguous_exact_match_does_not_guess_package_version() {
        let rows = rows_with_package_source("export function add(a,b){return a+b}");
        let package_sources = [
            PackageSource::external(
                "pkg",
                "1.2.3",
                "pkg/add",
                "add.js",
                "export function add(a, b) { return a + b; }",
            ),
            PackageSource::external(
                "pkg",
                "2.0.0",
                "pkg/add",
                "add.js",
                "export function add(a, b) { return a + b; }",
            ),
        ];

        let report = ExactPackageMatcher.match_rows(&rows, &package_sources);

        assert!(report.attributions.is_empty());
        assert!(report.audit.has(FindingCode::AmbiguousPackageMatch));
    }

    #[test]
    fn matcher_and_generation_share_source_slice_semantics() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some("export const one = 1;\nexport const two = 2;".to_string()),
        ));
        rows.modules.push(
            ModuleInput::package(ModuleId(10), "one", "pkg/one.ts", "pkg", None)
                .with_source_file(1)
                .with_source_span(SourceSpan::new(0, 21)),
        );
        rows.modules.push(
            ModuleInput::package(ModuleId(11), "two", "pkg/two.ts", "pkg", None)
                .with_source_file(1)
                .with_source_span(SourceSpan::new(22, 43)),
        );
        let package_sources = [PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/two",
            "two.js",
            "export const two = 2;",
        )];

        let report = ExactPackageMatcher.match_rows(&rows, &package_sources);

        assert_eq!(report.attributions.len(), 1);
        assert_eq!(report.attributions[0].module_id, ModuleId(11));
    }

    #[test]
    fn accepted_package_attribution_is_not_recomputed_in_parallel() {
        let mut rows = rows_with_package_source("export function add(a,b){return a+b}");
        rows.package_attributions
            .push(PackageAttributionInput::accepted_external(
                ModuleId(10),
                "pkg",
                "1.2.3",
                "pkg/add",
            ));
        let package_sources = [PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/add",
            "add.js",
            "export function add(a, b) { return a + b; }",
        )];

        let report = ExactPackageMatcher.match_rows(&rows, &package_sources);

        assert!(report.attributions.is_empty());
        assert!(report.matches.is_empty());
        assert!(report.audit.is_clean());
    }

    #[test]
    fn package_source_index_uses_binary_lookup_over_sorted_hashes() {
        let package_sources = [
            PackageSource::external("pkg", "1.0.0", "pkg/a", "a.js", "export const a = 1;"),
            PackageSource::external(
                "pkg",
                "2.0.0",
                "pkg/target",
                "target.js",
                "export const target = 42;",
            ),
            PackageSource::external("pkg", "3.0.0", "pkg/z", "z.js", "export const z = 26;"),
        ];
        let mut audit = Default::default();
        let index = index_package_sources(&package_sources, &mut audit);
        let rows = rows_with_package_source("export const target=42");
        let report = ExactPackageMatcher.match_rows(&rows, &package_sources);

        assert!(audit.is_clean());
        assert_eq!(index.entries.len(), 3);
        assert_eq!(report.attributions.len(), 1);
        assert_eq!(
            report.attributions[0].package_version.as_deref(),
            Some("2.0.0")
        );
    }
}
