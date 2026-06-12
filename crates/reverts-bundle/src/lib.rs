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

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles_and_links() {
        // Sentinel test — proves the crate is wired into the workspace.
    }
}
