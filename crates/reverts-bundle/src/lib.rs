//! Bundler-aware module extraction.
//!
//! Recognises bundler-specific wrapper shapes in JavaScript bundle source
//! and produces `InnerModule` records whose `body_span` always slices a
//! parseable program unit. See ADR 0004 for the architectural rationale.

mod classification;
pub mod classifier;
pub mod detectors;
mod inner_module;
pub mod merge;

pub use classification::{BundleClassification, IifeMetadata, MarkedMetadata};
pub use inner_module::{BundlerKind, InnerModule};
pub use merge::{MergeOutput, merge_classification};

use std::collections::HashMap;
use std::path::Path;

use reverts_input::{InputRows, ModuleInput, SourceFileInput};
use reverts_ir::ModuleId;
use reverts_observe::{AuditFinding, AuditReport, FindingCode};

/// Result of running the extractor over an entire `InputRows`.
#[derive(Debug, Clone, PartialEq)]
pub struct BundleExtraction {
    /// Classifications keyed by source_file_id.
    pub classifications: std::collections::BTreeMap<u32, BundleClassification>,
    /// New ModuleInput rows that should be appended to the bundle.
    pub new_modules: Vec<ModuleInput>,
    /// Updated module rows replacing entries in `input.modules`.
    pub updated_modules: Vec<ModuleInput>,
    /// Audit findings (BundleDetectorAmbiguous, MissingParseableBody, …).
    pub audit: AuditReport,
}

impl BundleExtraction {
    /// Apply the extraction into `input` in place. Replaces rows in
    /// `input.modules` whose ids appear in `updated_modules` and
    /// appends every `new_modules` row.
    pub fn merge_into(self, input: &mut InputRows) {
        let mut updates: HashMap<ModuleId, ModuleInput> = self
            .updated_modules
            .into_iter()
            .map(|m| (m.id, m))
            .collect();
        for module in input.modules.iter_mut() {
            if let Some(replacement) = updates.remove(&module.id) {
                *module = replacement;
            }
        }
        input.modules.extend(self.new_modules);
    }
}

/// Run bundler-aware module extraction on every provided source file.
/// Each source file is classified and its modules merged via
/// `merge_classification`. The aggregate `BundleExtraction` lets the
/// caller apply changes in one shot.
#[must_use]
pub fn extract(source_files: &[SourceFileInput], modules: &[ModuleInput]) -> BundleExtraction {
    let mut classifications = std::collections::BTreeMap::new();
    let mut new_modules = Vec::new();
    let mut updated_modules = Vec::new();
    let mut audit = AuditReport::default();

    let modules_by_id: HashMap<ModuleId, &ModuleInput> =
        modules.iter().map(|module| (module.id, module)).collect();

    // Synthetic module IDs must not collide with any real upstream ID.
    // Start at one past the largest real ID and increment for each new
    // row produced by `merge_classification`. Overflowing a `u32` here
    // would require > 4 billion modules — astronomically out of range
    // for any real bundle, but we still saturate-checked-add below so a
    // pathological input fails loudly rather than silently aliasing.
    let max_real_id = modules.iter().map(|m| m.id.0).max().unwrap_or(0);
    let mut next_synthetic_id = max_real_id.saturating_add(1);

    for source_file in source_files {
        if !is_bundle_candidate_path(Path::new(source_file.path.as_str())) {
            classifications.insert(source_file.id, BundleClassification::Plain);
            continue;
        }
        let Some(source) = source_file.source.as_deref() else {
            continue;
        };
        let classification = match classifier::classify(Path::new(&source_file.path), source) {
            Ok(classification) => classification,
            Err(message) => {
                audit.push(
                    AuditFinding::error(
                        FindingCode::AstFactExtractionFailed,
                        format!(
                            "bundle classifier could not parse {}: {message}",
                            source_file.path
                        ),
                    )
                    .with_module(source_file.id.to_string()),
                );
                continue;
            }
        };
        let merge_output = merge::merge_classification(
            source_file.id,
            modules,
            &classification,
            next_synthetic_id,
        );
        let added = u32::try_from(merge_output.new_modules.len())
            .expect("new_modules per source file fit in u32");
        next_synthetic_id = next_synthetic_id
            .checked_add(added)
            .expect("synthetic ModuleId space exhausted");
        for m in &merge_output.updated_modules {
            // Only collect modules that differ from upstream.
            if let Some(orig) = modules_by_id.get(&m.id)
                && orig.source_span != m.source_span
            {
                updated_modules.push(m.clone());
            }
        }
        new_modules.extend(merge_output.new_modules);
        audit.extend(merge_output.audit);
        classifications.insert(source_file.id, classification);
    }

    BundleExtraction {
        classifications,
        new_modules,
        updated_modules,
        audit,
    }
}

fn is_bundle_candidate_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["js", "mjs", "cjs", "jsx"]
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles_and_links() {
        // Sentinel test — proves the crate is wired into the workspace.
    }
}

#[cfg(test)]
mod public_api_tests {
    use super::*;
    use reverts_input::{ProjectInput, SourceFileInput};

    #[test]
    fn extract_plain_source_yields_no_modifications() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "plain.js",
            Some("function f() {}".into()),
        ));
        let extraction = extract(&rows.source_files, &rows.modules);
        assert!(extraction.new_modules.is_empty());
        assert!(extraction.updated_modules.is_empty());
        assert!(extraction.audit.is_clean());
        assert_eq!(
            extraction.classifications.get(&1),
            Some(&BundleClassification::Plain)
        );
    }

    #[test]
    fn extract_parse_error_records_audit_without_plain_classification() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "broken.js",
            Some("function bad( { )".into()),
        ));

        let extraction = extract(&rows.source_files, &rows.modules);

        assert!(extraction.new_modules.is_empty());
        assert!(extraction.updated_modules.is_empty());
        assert!(extraction.audit.has(FindingCode::AstFactExtractionFailed));
        assert!(!extraction.classifications.contains_key(&1));
    }

    #[test]
    fn extract_skips_typescript_sources_without_js_parse_audit() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/add.ts",
            Some("export function add(a: number, b: number) { return a + b; }".into()),
        ));

        let extraction = extract(&rows.source_files, &rows.modules);

        assert!(extraction.new_modules.is_empty());
        assert!(extraction.updated_modules.is_empty());
        assert!(extraction.audit.is_clean());
        assert_eq!(
            extraction.classifications.get(&1),
            Some(&BundleClassification::Plain)
        );
    }

    #[test]
    fn extract_esbuild_bundle_produces_new_module() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        let src = r#"var __commonJS=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports); var x = __commonJS({"node_modules/lodash/index.js": (e, m) => { m.exports = 1; }});"#;
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(src.to_string())));
        let extraction = extract(&rows.source_files, &rows.modules);
        assert_eq!(extraction.new_modules.len(), 1);
        assert!(matches!(
            extraction.classifications.get(&1),
            Some(BundleClassification::Marked(_))
        ));
    }

    #[test]
    fn merge_into_applies_updates_and_new_rows() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some(r#"var x = __commonJS({"a": (e, m) => { m.exports = 1; }});"#.into()),
        ));
        let extraction = extract(&rows.source_files, &rows.modules);
        let added = extraction.new_modules.len();
        extraction.merge_into(&mut rows);
        assert_eq!(rows.modules.len(), added);
    }
}
