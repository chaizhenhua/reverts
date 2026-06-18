use std::collections::{BTreeMap, BTreeSet};

use reverts_input::{
    InputRows, PackageAttributionInput, PackageAttributionStatus, PackageEmissionMode,
};
use reverts_ir::{ModuleId, ModuleKind};
use reverts_observe::{AuditFinding, AuditReport, FindingCode};

use crate::index::{PackageVersionIndex, fingerprint_modules_for_package};
use crate::model::{
    BestVersionMatch, ModuleMatchFingerprint, PackageMatch, PackageSource,
    VersionedPackageMatchReport, VersionedPackageMatcherConfig,
};
use crate::package_helpers::is_exact_package_version_hint;
use crate::scoring::accepted_attribution_from_match;
use crate::source::cache_surfaces::append_cache_anchored_package_surfaces;
use crate::source::source_imports::resolve_source_package_surfaces;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
/// Package matcher that scores concrete package versions before emitting attributions.
pub struct VersionedPackageMatcher {
    config: VersionedPackageMatcherConfig,
}

impl VersionedPackageMatcher {
    #[must_use]
    pub fn new(config: VersionedPackageMatcherConfig) -> Self {
        Self { config }
    }

    /// Matches unresolved package modules for a caller-supplied package-name
    /// subset. An empty subset intentionally performs no module matching.
    #[must_use]
    pub fn match_rows_for_packages(
        &self,
        rows: &InputRows,
        package_sources: &[PackageSource],
        package_names: &BTreeSet<String>,
    ) -> VersionedPackageMatchReport {
        self.match_rows_inner(rows, package_sources, Some(package_names))
    }

    /// Matches unresolved package modules to the best concrete package version.
    #[must_use]
    pub fn match_rows(
        &self,
        rows: &InputRows,
        package_sources: &[PackageSource],
    ) -> VersionedPackageMatchReport {
        self.match_rows_inner(rows, package_sources, None)
    }

    fn match_rows_inner(
        &self,
        rows: &InputRows,
        package_sources: &[PackageSource],
        package_filter: Option<&BTreeSet<String>>,
    ) -> VersionedPackageMatchReport {
        let mut audit = AuditReport::default();
        let index = PackageVersionIndex::build(package_sources, &mut audit);
        let mut decisions = Vec::new();
        let mut matches = Vec::new();
        let mut attributions = Vec::new();

        for package_name in package_names_for_matching(rows, package_filter) {
            let module_fingerprints =
                fingerprint_modules_for_package(rows, package_name.as_str(), &mut audit);
            if module_fingerprints.is_empty() {
                continue;
            }

            let hinted_fingerprints = group_exact_version_fingerprints(
                &index,
                package_name.as_str(),
                module_fingerprints,
            );

            for (package_version, module_fingerprints) in hinted_fingerprints {
                let decision = index.match_exact_version_for_package(
                    package_name.as_str(),
                    package_version.as_str(),
                    &module_fingerprints,
                    &self.config,
                );
                collect_decision_outputs(&decision, &mut matches, &mut attributions, &mut audit);
                decisions.push(decision);
            }
        }
        let mut surfaces = resolve_source_package_surfaces(
            rows,
            &attributions,
            package_sources,
            package_filter,
            &mut audit,
        );
        append_cache_anchored_package_surfaces(
            &mut surfaces,
            &attributions,
            package_sources,
            package_filter,
        );

        VersionedPackageMatchReport {
            attributions,
            surfaces,
            matches,
            version_matches: decisions,
            audit,
        }
    }
}

fn group_exact_version_fingerprints(
    index: &PackageVersionIndex<'_>,
    package_name: &str,
    module_fingerprints: Vec<ModuleMatchFingerprint>,
) -> BTreeMap<String, Vec<ModuleMatchFingerprint>> {
    let mut hinted = BTreeMap::<String, Vec<ModuleMatchFingerprint>>::new();
    for fingerprint in module_fingerprints {
        let hinted_version = fingerprint
            .package_version
            .as_deref()
            .map(str::trim)
            .filter(|version| is_exact_package_version_hint(version))
            .filter(|version| index.has_package_version(package_name, version))
            .map(ToOwned::to_owned);
        if let Some(package_version) = hinted_version {
            hinted.entry(package_version).or_default().push(fingerprint);
        }
    }
    hinted
}

fn collect_decision_outputs(
    decision: &BestVersionMatch,
    matches: &mut Vec<PackageMatch>,
    attributions: &mut Vec<PackageAttributionInput>,
    audit: &mut AuditReport,
) {
    if let BestVersionMatch::Selected {
        score: _score,
        module_matches,
    } = decision
    {
        for module_match in module_matches {
            if module_match.external_importable {
                attributions.push(accepted_attribution_from_match(module_match));
            }
            matches.push(PackageMatch::from_module_match(module_match));
        }
    } else if let BestVersionMatch::Ambiguous {
        package_name,
        scores: _scores,
    } = decision
    {
        audit.push(
            AuditFinding::error(
                FindingCode::AmbiguousPackageMatch,
                "package version matching found more than one best version",
            )
            .with_binding(package_name.clone()),
        );
    }
}

pub(crate) fn has_accepted_attribution(rows: &InputRows, module_id: ModuleId) -> bool {
    rows.package_attributions.iter().any(|attribution| {
        attribution.module_id == module_id
            && attribution.status == PackageAttributionStatus::Accepted
            && attribution.emission_mode == PackageEmissionMode::ExternalImport
    })
}

fn package_names_for_matching(
    rows: &InputRows,
    package_filter: Option<&BTreeSet<String>>,
) -> BTreeSet<String> {
    let mut names = rows
        .modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
        .filter(|module| !has_accepted_attribution(rows, module.id))
        .filter_map(|module| module.package_name.clone())
        .collect::<BTreeSet<_>>();
    if let Some(package_filter) = package_filter {
        names.retain(|package_name| package_filter.contains(package_name));
    }
    names
}
