use reverts_graph::AstWrapperKind;

use crate::inner_module::{BundlerKind, InnerModule};

/// Outcome of classifying a single source file. Drives downstream
/// behaviour: Plain flows through unchanged, Marked is split into
/// inner modules, Iife is reserved for monolithic vendored bundles
/// recovered via clustering (Phase γ).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleClassification {
    /// No bundler wrapper recognised. Source flows through as one
    /// module with no inner subdivision.
    Plain,
    /// Bundler wrapper recognised. Carries the extracted inner-module
    /// list and the bundler kind that produced it.
    Marked(MarkedMetadata),
    /// Monolithic IIFE-shaped vendored bundle. Phase α records the
    /// wrapper shape but does not recover inner clusters (Phase γ
    /// fills `inner_clusters`).
    Iife(IifeMetadata),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkedMetadata {
    /// Inner modules in source-text order.
    pub inner_modules: Vec<InnerModule>,
    /// Bundler whose detector emitted these inner modules. Multiple
    /// detectors may match the same file; the highest-confidence
    /// detector's results are used and runners-up emit
    /// `BundleDetectorAmbiguous` audit findings.
    pub detected_by: BundlerKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IifeMetadata {
    /// IIFE wrapper shape recognised at the top level of the file.
    pub wrapper_kind: AstWrapperKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_are_distinguishable() {
        let plain = BundleClassification::Plain;
        let marked = BundleClassification::Marked(MarkedMetadata {
            inner_modules: vec![],
            detected_by: BundlerKind::Esbuild,
        });
        let iife = BundleClassification::Iife(IifeMetadata {
            wrapper_kind: AstWrapperKind::FunctionIife,
        });
        assert_ne!(plain, marked);
        assert_ne!(marked, iife);
        assert_ne!(plain, iife);
    }

    #[test]
    fn marked_metadata_round_trips_through_variant() {
        let meta = MarkedMetadata {
            inner_modules: vec![],
            detected_by: BundlerKind::Webpack5,
        };
        let classification = BundleClassification::Marked(meta.clone());
        let BundleClassification::Marked(extracted) = classification else {
            panic!("expected Marked variant");
        };
        assert_eq!(extracted, meta);
    }

    #[test]
    fn iife_metadata_round_trips_through_variant() {
        let meta = IifeMetadata {
            wrapper_kind: AstWrapperKind::ArrowIife,
        };
        let classification = BundleClassification::Iife(meta.clone());
        let BundleClassification::Iife(extracted) = classification else {
            panic!("expected Iife variant");
        };
        assert_eq!(extracted, meta);
    }
}
