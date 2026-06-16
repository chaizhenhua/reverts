use reverts_input::{PackageAttributionInput, PackageSurfaceInput};
use reverts_observe::AuditReport;

use super::{BestVersionMatch, PackageMatch};

#[derive(Debug, Clone, PartialEq)]
/// Result of a versioned package matching pass.
pub struct VersionedPackageMatchReport {
    /// Accepted attributions that can be persisted by the caller.
    pub attributions: Vec<PackageAttributionInput>,
    /// Accepted project-level package surfaces discovered from source-backed bare imports.
    pub surfaces: Vec<PackageSurfaceInput>,
    /// Match evidence for accepted attributions.
    pub matches: Vec<PackageMatch>,
    /// Per package or package-version best-version decisions.
    pub version_matches: Vec<BestVersionMatch>,
    /// Ambiguity, missing source, and parse findings.
    pub audit: AuditReport,
}

#[derive(Debug, Clone, PartialEq)]
/// Unified package matching pipeline output.
///
/// This is the single matcher-side orchestration point for the module/source
/// version matcher, the function-level cascade matcher, structural-bag
/// ownership, and dependency-closure ownership promotion.
pub struct PackageMatchingPipelineReport {
    /// Module-level package attributions/surfaces plus all promoted ownership
    /// matches that generation and persistence consume. Matcher-stage audit
    /// findings are merged here so callers do not have to wire side reports.
    pub package_report: VersionedPackageMatchReport,
    /// Function-level package evidence produced while matching. These rows are
    /// diagnostics/persistence evidence, not a second module-generation path.
    pub function_attributions: Vec<PackageAttributionInput>,
    /// Count of function-level ownership matches, including source-only
    /// evidence that cannot be emitted as an external import.
    pub function_ownership_matches: usize,
}
