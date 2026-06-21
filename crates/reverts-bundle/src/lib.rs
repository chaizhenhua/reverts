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
    /// In-memory synthetic source files backing reconstructed modules
    /// (esbuild multi-handle handles). Append these to the bundle's
    /// `source_files` BEFORE `new_modules` so the module FK resolves.
    pub new_source_files: Vec<SourceFileInput>,
    /// Updated module rows replacing entries in `input.modules`.
    pub updated_modules: Vec<ModuleInput>,
    /// Audit findings (BundleDetectorAmbiguous, MissingParseableBody, …).
    pub audit: AuditReport,
}

impl BundleExtraction {
    /// Apply the extraction into `input` in place. Replaces rows in
    /// `input.modules` whose ids appear in `updated_modules`, appends every
    /// `new_source_files` row, then appends every `new_modules` row.
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
        input.source_files.extend(self.new_source_files);
        input.modules.extend(self.new_modules);
    }
}

/// Run bundler-aware module extraction on every provided source file.
/// Each source file is classified and its modules merged via
/// `merge_classification`. The aggregate `BundleExtraction` lets the
/// caller apply changes in one shot.
#[must_use]
pub fn extract(source_files: &[SourceFileInput], modules: &[ModuleInput]) -> BundleExtraction {
    extract_with_reserved_ids(source_files, modules, 0)
}

/// Like [`extract`], but reserves the id range `0..=reserved_max_id` so the
/// synthetic module- and source-file-id allocators start past it. The matcher
/// loader drops modules whose `file_id` is not in `project_files` (legacy
/// half-persisted reconstructions), so those orphan ids are invisible to the
/// `modules` slice here; persisting a synthetic source onto one of them
/// resurrects the orphan module against a mismatched span. Callers with DB
/// access pass `MAX(modules.id, modules.file_id)` over the WHOLE table to keep
/// the synthetic id space globally disjoint.
#[must_use]
pub fn extract_with_reserved_ids(
    source_files: &[SourceFileInput],
    modules: &[ModuleInput],
    reserved_max_id: u32,
) -> BundleExtraction {
    let mut classifications = std::collections::BTreeMap::new();
    let mut new_modules = Vec::new();
    let mut new_source_files = Vec::new();
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
    let max_real_id = modules
        .iter()
        .map(|m| m.id.0)
        .max()
        .unwrap_or(0)
        .max(reserved_max_id);
    let mut next_synthetic_id = max_real_id.saturating_add(1);
    // Synthetic SOURCE FILE ids for reconstructed multi-handle modules,
    // allocated past the largest real source file id (a separate namespace
    // from module ids). The caller may pass only a SUBSET of source files
    // (generation classifies just the module-less ones), so also consider the
    // source_file_ids referenced by `modules` — together they cover every real
    // source file id and prevent a synthetic id from aliasing an existing file.
    let max_source_file_id = source_files
        .iter()
        .map(|sf| sf.id)
        .chain(modules.iter().filter_map(|m| m.source_file_id))
        .max()
        .unwrap_or(0)
        .max(reserved_max_id);
    let mut next_synthetic_source_file_id = max_source_file_id.saturating_add(1);

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
                // Bundle classifier parse failure means we can't split this
                // source into inner modules. Per ADR 0002 we surface the
                // failure as a warning; the source remains as a single
                // unsplit module and the rest of the pipeline can still
                // run on the project.
                audit.push(
                    AuditFinding::warning(
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
            next_synthetic_source_file_id,
        );
        let added = u32::try_from(merge_output.new_modules.len())
            .expect("new_modules per source file fit in u32");
        next_synthetic_id = next_synthetic_id
            .checked_add(added)
            .expect("synthetic ModuleId space exhausted");
        let added_source_files = u32::try_from(merge_output.new_source_files.len())
            .expect("new_source_files per source file fit in u32");
        next_synthetic_source_file_id = next_synthetic_source_file_id
            .checked_add(added_source_files)
            .expect("synthetic source file id space exhausted");
        for m in &merge_output.updated_modules {
            // Only collect modules that differ from upstream.
            if let Some(orig) = modules_by_id.get(&m.id)
                && orig.source_span != m.source_span
            {
                updated_modules.push(m.clone());
            }
        }
        new_modules.extend(merge_output.new_modules);
        new_source_files.extend(merge_output.new_source_files);
        audit.extend(merge_output.audit);
        classifications.insert(source_file.id, classification);
    }

    BundleExtraction {
        classifications,
        new_modules,
        new_source_files,
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
    fn reserved_id_floor_pushes_synthetic_ids_past_orphan_space() {
        // Multi-handle esbuild var statement reconstructs per-handle synthetic
        // SOURCE files. The matcher loader drops orphan modules whose file_id
        // is absent from project_files, so their ids are invisible here; the
        // reserved floor keeps the synthetic allocator from aliasing them and
        // resurrecting an orphan against a mismatched span on the next load.
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        let src = "var st=(A,Q)=>()=>(A&&(Q=A(A=0)),Q);\n\
                   var a,X=st(()=>{a=1}),b,c,Y=st(()=>{b=2;c=3});";
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(src.to_string())));

        let reserved = 5000;
        let extraction = extract_with_reserved_ids(&rows.source_files, &rows.modules, reserved);

        assert!(
            !extraction.new_source_files.is_empty(),
            "multi-handle reconstruction should emit synthetic source files"
        );
        for sf in &extraction.new_source_files {
            assert!(
                sf.id > reserved,
                "synthetic source file id {} must exceed reserved floor {reserved}",
                sf.id
            );
        }
        for module in &extraction.new_modules {
            assert!(
                module.id.0 > reserved,
                "synthetic module id {} must exceed reserved floor {reserved}",
                module.id.0
            );
        }
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
