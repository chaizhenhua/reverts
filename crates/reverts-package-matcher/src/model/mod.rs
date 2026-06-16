mod fingerprint;
mod matching;
mod proof_target;
mod report;
mod source;

pub use fingerprint::{ModuleMatchFingerprint, PackageSourceFingerprint, PackageVersionCandidate};
pub(crate) use matching::package_module_source_quality_label;
pub use matching::{
    BestVersionMatch, ModuleMatchStrategy, ModulePackageMatch, PackageMatch,
    PackageModuleSourceQuality, VersionMatchScore, VersionedPackageMatcherConfig,
};
pub(crate) use proof_target::{
    ConcretePackageSourcePath, CorrectedPackageExternalImportTarget, ExternalImportTarget,
};
pub use report::{PackageMatchingPipelineReport, VersionedPackageMatchReport};
pub(crate) use source::PACKAGE_SOURCE_FINGERPRINT_MAX_BYTES;
pub use source::{
    PackageImportSite, PackagePublicExportProof, PackageSource, SourcePackageImportParseError,
};
