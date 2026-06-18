//! Shared mutation for turning a package-source proof into an external import.
//!
//! Ownership promotion and proof-driven externalization used to each hand-roll
//! the same three mutations: update the package match, append an accepted
//! external attribution, and derive the subpath. Keeping that write in one
//! place makes the promotion mechanism explicit while leaving each caller to
//! own only its proof strategy.

use reverts_input::PackageAttributionInput;
use reverts_ir::{ModuleId, split_bare_specifier};

use crate::{ModuleMatchStrategy, PackageMatch, VersionedPackageMatchReport};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExternalImportPromotion {
    pub(crate) module_id: ModuleId,
    pub(crate) package_name: String,
    pub(crate) package_version: String,
    pub(crate) export_specifier: String,
    pub(crate) resolved_file: String,
    pub(crate) strategy: ModuleMatchStrategy,
    pub(crate) function_signature_matches: usize,
    pub(crate) string_anchor_matches: usize,
}

impl ExternalImportPromotion {
    #[must_use]
    pub(crate) fn attribution(&self) -> PackageAttributionInput {
        let mut attribution = PackageAttributionInput::accepted_external(
            self.module_id,
            self.package_name.as_str(),
            self.package_version.as_str(),
            self.export_specifier.as_str(),
        )
        .with_resolved_file(self.resolved_file.as_str());
        if let Some((_package_name, Some(subpath))) =
            split_bare_specifier(self.export_specifier.as_str())
        {
            attribution = attribution.with_subpath(subpath);
        }
        attribution
    }

    #[must_use]
    fn package_match(self) -> PackageMatch {
        PackageMatch {
            module_id: self.module_id,
            package_name: self.package_name,
            package_version: self.package_version,
            export_specifier: self.export_specifier,
            source_path: self.resolved_file,
            normalized_source_hash: String::new(),
            strategy: self.strategy,
            function_signature_matches: self.function_signature_matches,
            string_anchor_matches: self.string_anchor_matches,
            external_importable: true,
        }
    }
}

pub(crate) fn apply_external_import_promotion(
    report: &mut VersionedPackageMatchReport,
    match_index: Option<usize>,
    promotion: ExternalImportPromotion,
) {
    let attribution = promotion.attribution();
    match match_index {
        Some(index) => {
            if let Some(package_match) = report.matches.get_mut(index) {
                package_match.package_name = promotion.package_name;
                package_match.package_version = promotion.package_version;
                package_match.export_specifier = promotion.export_specifier;
                package_match.source_path = promotion.resolved_file;
                package_match.function_signature_matches = promotion.function_signature_matches;
                package_match.string_anchor_matches = promotion.string_anchor_matches;
                package_match.external_importable = true;
            }
        }
        None => report.matches.push(promotion.package_match()),
    }
    report.attributions.push(attribution);
}
