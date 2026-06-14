pub mod acceptance;
pub mod cascade;
pub mod cascade_match;
pub mod hungarian;
pub mod package_helpers;
pub mod structural_bag;
pub mod tier;
pub mod variant;
pub mod version;

pub use acceptance::{AcceptanceDecision, classify};
pub use cascade::{GlobalAssignment, assign_globally, cascade_candidates, match_function};
pub use cascade_match::{CascadeMatchReport, CascadeOwnershipMatch, match_with_cascade};
pub use hungarian::assign_max_weight;
pub use package_helpers::{
    direct_module_dependencies, direct_module_dependents, has_accepted_external_attribution,
    is_exact_package_version_hint, is_json_source_path, ownership_by_module,
    package_semantic_path_prefixes, path_hint_tokens, strip_source_extension,
};
pub use structural_bag::{
    StructuralBagMatchReport, match_structural_bags, match_structural_bags_with_excluded_modules,
};
pub use tier::{
    FunctionMatch, STRUCTURAL_FREQUENCY_LIMIT, try_exact, try_exact_alternate,
    try_feature_similarity, try_structural_anchored, try_structural_only,
};
pub use variant::{VariantSelection, pick_variants};
pub use version::{
    BestVersionDecision, VERSION_AMBIGUITY_EPSILON, VERSION_INSUFFICIENT_THRESHOLD, pick_versions,
};

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::{
    AstKind, Visit,
    ast::{
        Argument, ArrowFunctionExpression, CallExpression, ExportAllDeclaration,
        ExportNamedDeclaration, Expression, ImportDeclaration, ImportExpression, TemplateElement,
    },
    visit::walk::{
        walk_call_expression, walk_export_all_declaration, walk_export_named_declaration,
        walk_import_expression, walk_template_element,
    },
};
use oxc_parser::Parser;
use reverts_graph::FunctionExtractor;
use reverts_input::{
    InputRows, ModuleDependencyTarget, ModuleInput, PackageAttributionInput,
    PackageAttributionStatus, PackageEmissionMode, PackageSurfaceInput,
};
use reverts_ir::hash::{
    FNV_OFFSET_BASIS, fnv1a_hex as stable_hash, update_fnv1a as update_stable_hash,
};
use reverts_ir::{
    ModuleId, ModuleKind, NormalizationPassId, is_valid_package_name, split_bare_specifier,
};
use reverts_js::normalize::{apply_to_source, stable_passes};
use reverts_js::{
    JsError, ParseError, ParseGoal, normalize_source_for_pipeline, parse_error_message,
    parse_options_for, source_type_candidates,
};
use reverts_observe::{AuditFinding, AuditReport, FindingCode};
use reverts_package::is_node_builtin;
use semver::Version;

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
    /// Whether a match against this source may be emitted as an external import.
    ///
    /// Full package source roots often include private/internal files that are
    /// useful for ownership matching but are not guaranteed to be importable
    /// through a package's `exports` map. Those sources must stay source-only
    /// until an import-shape resolver proves the specifier is safe.
    pub external_importable: bool,
}

const PACKAGE_SOURCE_FINGERPRINT_MAX_BYTES: usize = 512 * 1024;

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
            external_importable: true,
        }
    }

    /// Creates a package source candidate used only for ownership/source
    /// matching. Matches against this source are reported but are not turned
    /// into accepted `external_import` attributions.
    #[must_use]
    pub fn source_only(
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
            external_importable: false,
        }
    }

    pub(crate) fn is_within_fingerprint_budget(&self) -> bool {
        self.source.len() <= PACKAGE_SOURCE_FINGERPRINT_MAX_BYTES
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
/// Source-backed bare package import/require site discovered in the original bundle source.
pub struct PackageImportSite {
    /// Source file id that contains the import expression.
    pub source_file_id: u32,
    /// Original source path.
    pub source_file_path: String,
    /// npm package name parsed from the bare specifier.
    pub package_name: String,
    /// Concrete bare specifier used by source, e.g. `undici` or `lodash/map`.
    pub specifier: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Structural fingerprint used to compare one bundle module with package source candidates.
pub struct ModuleMatchFingerprint {
    /// module id in the input bundle.
    pub module_id: ModuleId,
    /// optional package hint attached to the module.
    pub package_name: Option<String>,
    /// optional concrete package version hint attached to the module.
    pub package_version: Option<String>,
    /// stable hash of the AST-normalized source body.
    pub normalized_source_hash: String,
    /// Stable hashes of normalized source variants produced by one
    /// normalization pass. Includes `normalized_source_hash`.
    pub normalized_source_hashes: BTreeSet<String>,
    /// AST-derived function signature hashes.
    pub function_signature_hashes: BTreeSet<String>,
    /// string literal anchors collected from the AST.
    pub string_anchors: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Fingerprint for one cached package source file.
pub struct PackageSourceFingerprint<'a> {
    /// cached source candidate.
    pub source: &'a PackageSource,
    /// stable hash of the AST-normalized source body.
    pub normalized_source_hash: String,
    /// Stable hashes of normalized source variants produced by one
    /// normalization pass. Includes `normalized_source_hash`.
    pub normalized_source_hashes: BTreeSet<String>,
    /// AST-derived function signature hashes.
    pub function_signature_hashes: BTreeSet<String>,
    /// string literal anchors collected from the AST.
    pub string_anchors: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Version bucket for one package.
pub struct PackageVersionCandidate<'a> {
    /// npm package name.
    pub package_name: String,
    /// concrete package version.
    pub package_version: String,
    /// cached source files belonging to this package version.
    pub sources: Vec<PackageSourceFingerprint<'a>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Strategy that proved a module-to-package-source match.
pub enum ModuleMatchStrategy {
    /// Full module source identity after AST normalization.
    NormalizedSourceHash,
    /// Function signatures plus string anchors matched the package source.
    FunctionSignatureAndStringAnchors,
    /// Function signatures plus string anchors matched the package version as
    /// an aggregate, but not one unique importable source file. This proves
    /// package ownership only.
    AggregateFunctionSignatureAndStringAnchors,
    /// Every function fingerprint in the module was attributed to one package
    /// version by the cascade matcher.
    CascadeFunctionCoverage,
    /// A dominant subset of function fingerprints in the module was
    /// attributed to one package version by the cascade matcher. This proves
    /// package ownership only; it is not sufficient to externalize the whole
    /// module as an import.
    CascadePartialFunctionCoverage,
    /// Aggregate structural fingerprint axes matched one package version. This
    /// proves package ownership only and intentionally does not prove a unique
    /// importable source file.
    AggregateStructuralBagSimilarity,
    /// Direct module dependencies are already owned by one package version.
    /// This proves ownership for dependency-only bundle wrappers/barrels, but
    /// not a safe single external import.
    DependencyClosureOwnership,
}

impl ModuleMatchStrategy {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NormalizedSourceHash => "normalized_source_hash",
            Self::FunctionSignatureAndStringAnchors => "function_signature_and_string_anchors",
            Self::AggregateFunctionSignatureAndStringAnchors => {
                "aggregate_function_signature_and_string_anchors"
            }
            Self::CascadeFunctionCoverage => "cascade_function_coverage",
            Self::CascadePartialFunctionCoverage => "cascade_partial_function_coverage",
            Self::AggregateStructuralBagSimilarity => "aggregate_structural_bag_similarity",
            Self::DependencyClosureOwnership => "dependency_closure_ownership",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Evidence that one module matched one source inside a package version.
pub struct ModulePackageMatch {
    /// matched module id.
    pub module_id: ModuleId,
    /// matched npm package name.
    pub package_name: String,
    /// matched concrete package version.
    pub package_version: String,
    /// accepted import specifier.
    pub export_specifier: String,
    /// cached package source path.
    pub source_path: String,
    /// strategy that proved the match.
    pub strategy: ModuleMatchStrategy,
    /// stable hash of the normalized matched source.
    pub normalized_source_hash: String,
    /// overlapping function signatures.
    pub function_signature_matches: usize,
    /// overlapping string anchors.
    pub string_anchor_matches: usize,
    /// Whether this match is safe to turn into an external import attribution.
    pub external_importable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Aggregate score for one package version.
pub struct VersionMatchScore {
    /// npm package name.
    pub package_name: String,
    /// concrete package version.
    pub package_version: String,
    /// package modules that were considered for this package.
    pub total_modules: usize,
    /// modules matched by any accepted strategy.
    pub matched_modules: usize,
    /// modules matched by normalized source identity.
    pub source_hash_matches: usize,
    /// total overlapping function signatures.
    pub function_signature_matches: usize,
    /// total overlapping string anchors.
    pub string_anchor_matches: usize,
    /// weighted score used for version ordering.
    pub score: u32,
    /// number of package versions probed by binary search before certification.
    pub binary_search_probes: usize,
}

impl VersionMatchScore {
    #[must_use]
    pub const fn has_evidence(&self) -> bool {
        self.score > 0 && self.matched_modules > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Selected or rejected best-version decision for one package.
pub enum BestVersionMatch {
    /// One concrete version has unique best evidence.
    Selected {
        /// selected score.
        score: VersionMatchScore,
        /// module-level source matches for the selected version.
        module_matches: Vec<ModulePackageMatch>,
    },
    /// More than one version has the same best score.
    Ambiguous {
        /// package name.
        package_name: String,
        /// best tied scores.
        scores: Vec<VersionMatchScore>,
    },
    /// No version produced usable evidence.
    NoMatch {
        /// package name.
        package_name: String,
        /// scores that were evaluated.
        scores: Vec<VersionMatchScore>,
    },
    /// Evidence exists but does not satisfy the configured acceptance threshold.
    InsufficientEvidence {
        /// strongest score.
        score: VersionMatchScore,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Configuration for version-level package matching.
pub struct VersionedPackageMatcherConfig {
    /// Minimum function signature overlap for non-source-identity matches.
    pub min_function_signature_matches: usize,
    /// Minimum string anchor overlap for non-source-identity matches.
    pub min_string_anchor_matches: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Coarse trust classification for a source slice attached to a package module.
///
/// `Invalid` slices are not parseable as a standalone module body and are
/// usually bad byte spans recovered from a bundle. They are excluded from
/// source/hash and cascade matching so they do not pollute package evidence.
/// `Weak` slices are parseable but do not contain any strong token from the
/// package path hint; the exact source/hash matcher excludes them, while
/// callers may still feed them to weaker ownership-only paths such as cascade
/// because minification often erases source-path names.
pub enum PackageModuleSourceQuality {
    Trusted,
    Weak,
    Invalid,
}

#[must_use]
fn package_module_source_quality_label(quality: PackageModuleSourceQuality) -> &'static str {
    match quality {
        PackageModuleSourceQuality::Trusted => "trusted",
        PackageModuleSourceQuality::Weak => "weak",
        PackageModuleSourceQuality::Invalid => "invalid",
    }
}

impl Default for VersionedPackageMatcherConfig {
    fn default() -> Self {
        Self {
            min_function_signature_matches: 2,
            min_string_anchor_matches: 1,
        }
    }
}

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
        let surfaces =
            resolve_source_package_surfaces(rows, &attributions, package_sources, package_filter);

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

impl PackageMatchingPipelineReport {
    #[must_use]
    fn empty() -> Self {
        Self {
            package_report: VersionedPackageMatchReport {
                attributions: Vec::new(),
                surfaces: Vec::new(),
                matches: Vec::new(),
                version_matches: Vec::new(),
                audit: AuditReport::default(),
            },
            function_attributions: Vec::new(),
            function_ownership_matches: 0,
        }
    }
}

/// Runs the complete package matching pipeline through one matcher-owned
/// entry point.
///
/// `package_filter = None` means match every package name discoverable from
/// the input. `Some(filter)` restricts every sub-pipeline to the supplied
/// package names.
#[must_use]
pub fn match_packages_with_pipeline(
    rows: &InputRows,
    package_sources: &[PackageSource],
    package_filter: Option<&BTreeSet<String>>,
) -> PackageMatchingPipelineReport {
    if package_sources.is_empty() && package_filter.is_none_or(BTreeSet::is_empty) {
        return PackageMatchingPipelineReport::empty();
    }

    let mut package_report = if let Some(package_filter) = package_filter {
        VersionedPackageMatcher::default().match_rows_for_packages(
            rows,
            package_sources,
            package_filter,
        )
    } else {
        VersionedPackageMatcher::default().match_rows(rows, package_sources)
    };

    let fingerprints_by_module = fingerprints_from_rows(rows, package_filter);
    let cascade_report =
        match_with_cascade_scoped_by_module_hints(rows, &fingerprints_by_module, package_sources);
    promote_cascade_function_coverage_to_module_attributions(
        rows,
        &fingerprints_by_module,
        &cascade_report,
        &mut package_report,
    );
    let function_attributions = cascade_report.attributions;
    let function_ownership_matches = cascade_report.ownership_matches.len();
    package_report.audit.extend(cascade_report.audit);

    let structural_bag_excluded_modules = package_report
        .matches
        .iter()
        .map(|package_match| package_match.module_id)
        .collect::<BTreeSet<_>>();
    let structural_bag_report = match_structural_bags_with_excluded_modules(
        rows,
        package_sources,
        package_filter,
        &structural_bag_excluded_modules,
    );
    promote_structural_bag_ownership_matches(
        rows,
        structural_bag_report.matches.as_slice(),
        &mut package_report,
    );
    package_report.audit.extend(structural_bag_report.audit);
    promote_exact_hint_ownership_matches(rows, package_sources, &mut package_report);
    promote_dependency_closure_ownership_matches(rows, &mut package_report);
    promote_dependency_cluster_ownership_matches(rows, &mut package_report);
    promote_package_file_graph_ownership_matches(rows, &mut package_report);

    PackageMatchingPipelineReport {
        package_report,
        function_attributions,
        function_ownership_matches,
    }
}

/// Builds per-module function fingerprints from raw input rows using the same
/// function-axis extractor that powers the cascade package-source index.
fn fingerprints_from_rows(
    rows: &InputRows,
    package_filter: Option<&BTreeSet<String>>,
) -> BTreeMap<ModuleId, Vec<reverts_ir::FunctionFingerprint>> {
    let mut out = BTreeMap::new();
    for module in &rows.modules {
        if let Some(package_filter) = package_filter
            && (module.kind != ModuleKind::Package
                || !module
                    .package_name
                    .as_deref()
                    .is_some_and(|package_name| package_filter.contains(package_name)))
        {
            continue;
        }
        if let Some(slice) = rows.module_source_slice(module.id) {
            if module.kind == ModuleKind::Package
                && package_module_source_quality(module, slice.source_file_path, slice.source)
                    == PackageModuleSourceQuality::Invalid
            {
                continue;
            }
            let fps = FunctionExtractor::fingerprint(module.id, slice.source);
            if !fps.is_empty() {
                out.insert(module.id, fps);
            }
        }
    }
    out
}

fn match_with_cascade_scoped_by_module_hints(
    rows: &InputRows,
    fingerprints_by_module: &BTreeMap<ModuleId, Vec<reverts_ir::FunctionFingerprint>>,
    package_sources: &[PackageSource],
) -> CascadeMatchReport {
    let modules_by_id = rows
        .modules
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let mut grouped_fingerprints = BTreeMap::<
        (Option<String>, Option<String>),
        BTreeMap<ModuleId, Vec<reverts_ir::FunctionFingerprint>>,
    >::new();
    for (module_id, fingerprints) in fingerprints_by_module {
        let scope = modules_by_id.get(module_id).and_then(|module| {
            if module.kind != ModuleKind::Package {
                return None;
            }
            let package_name = module.package_name.as_ref()?.trim();
            if package_name.is_empty() {
                return None;
            }
            let package_version = module
                .package_version
                .as_deref()
                .map(str::trim)
                .filter(|version| !version.is_empty())
                .map(ToString::to_string);
            Some((Some(package_name.to_string()), package_version))
        });
        grouped_fingerprints
            .entry(scope.unwrap_or((None, None)))
            .or_default()
            .insert(*module_id, fingerprints.clone());
    }

    let mut merged = CascadeMatchReport {
        attributions: Vec::new(),
        ownership_matches: Vec::new(),
        audit: AuditReport::default(),
    };
    for ((package_name, package_version), scoped_fingerprints) in grouped_fingerprints {
        let scoped_sources = package_sources
            .iter()
            .filter(|source| {
                package_name
                    .as_deref()
                    .is_none_or(|name| source.package_name == name)
                    && package_version
                        .as_deref()
                        .is_none_or(|version| source.package_version == version)
            })
            .cloned()
            .collect::<Vec<_>>();
        if scoped_sources.is_empty() {
            continue;
        }
        let report = match_with_cascade(&scoped_fingerprints, &scoped_sources);
        merged.attributions.extend(report.attributions);
        merged.ownership_matches.extend(report.ownership_matches);
        merged.audit.extend(report.audit);
    }
    merged
}

fn promote_cascade_function_coverage_to_module_attributions(
    rows: &InputRows,
    fingerprints_by_module: &BTreeMap<ModuleId, Vec<reverts_ir::FunctionFingerprint>>,
    cascade_report: &CascadeMatchReport,
    report: &mut VersionedPackageMatchReport,
) {
    let already_accepted = report
        .attributions
        .iter()
        .chain(rows.package_attributions.iter())
        .filter(|attribution| {
            attribution.status == PackageAttributionStatus::Accepted
                && attribution.emission_mode == PackageEmissionMode::ExternalImport
        })
        .map(|attribution| attribution.module_id)
        .collect::<BTreeSet<_>>();
    let matched_modules = report
        .matches
        .iter()
        .map(|package_match| package_match.module_id)
        .collect::<BTreeSet<_>>();
    let cascade_ownership_by_module = cascade_report.ownership_matches.iter().fold(
        BTreeMap::<ModuleId, Vec<&CascadeOwnershipMatch>>::new(),
        |mut by_module, ownership| {
            by_module
                .entry(ownership.module_id)
                .or_default()
                .push(ownership);
            by_module
        },
    );

    for module in &rows.modules {
        if module.kind != ModuleKind::Package
            || already_accepted.contains(&module.id)
            || matched_modules.contains(&module.id)
        {
            continue;
        }
        let Some(expected_package_name) = module.package_name.as_deref() else {
            continue;
        };
        let Some(fingerprints) = fingerprints_by_module.get(&module.id) else {
            continue;
        };
        let Some(cascade_ownership) = cascade_ownership_by_module.get(&module.id) else {
            continue;
        };
        if fingerprints.is_empty() {
            continue;
        }
        let mut ownership_by_package_version =
            BTreeMap::<(&str, &str), Vec<&CascadeOwnershipMatch>>::new();
        for ownership in cascade_ownership {
            ownership_by_package_version
                .entry((
                    ownership.package_name.as_str(),
                    ownership.package_version.as_str(),
                ))
                .or_default()
                .push(*ownership);
        }
        let mut ranked_ownership = ownership_by_package_version
            .into_iter()
            .map(|(package_version, ownership)| {
                let covered_spans = ownership
                    .iter()
                    .map(|ownership| ownership.function_span)
                    .collect::<BTreeSet<_>>();
                (package_version, ownership, covered_spans)
            })
            .collect::<Vec<_>>();
        ranked_ownership.sort_by(|left, right| {
            right
                .2
                .len()
                .cmp(&left.2.len())
                .then_with(|| left.0.cmp(&right.0))
        });
        let Some(((package_name, package_version), selected_ownership, covered_spans)) =
            ranked_ownership.first()
        else {
            continue;
        };
        let package_name = *package_name;
        let package_version = *package_version;
        if package_name != expected_package_name {
            continue;
        }
        if module
            .package_version
            .as_deref()
            .map(str::trim)
            .filter(|version| !version.is_empty())
            .is_some_and(|expected_version| package_version != expected_version)
        {
            continue;
        }

        let covered_count = covered_spans.len();
        let runner_up_count = ranked_ownership.get(1).map_or(0, |ranked| ranked.2.len());
        let is_full_coverage =
            covered_count == fingerprints.len() && cascade_ownership.len() == fingerprints.len();
        if !is_full_coverage
            && !accept_partial_cascade_coverage(
                fingerprints.len(),
                covered_count,
                cascade_ownership
                    .iter()
                    .map(|ownership| ownership.function_span)
                    .collect::<BTreeSet<_>>()
                    .len(),
                runner_up_count,
            )
        {
            continue;
        }

        let export_specifiers = selected_ownership
            .iter()
            .map(|ownership| ownership.export_specifier.as_str())
            .collect::<BTreeSet<_>>();
        let can_externalize = is_full_coverage
            && selected_ownership
                .iter()
                .all(|ownership| ownership.external_importable)
            && export_specifiers.len() == 1;
        let strategy = if is_full_coverage {
            ModuleMatchStrategy::CascadeFunctionCoverage
        } else {
            ModuleMatchStrategy::CascadePartialFunctionCoverage
        };
        let export_specifier = export_specifiers.first().copied().unwrap_or(package_name);

        if can_externalize {
            let mut attribution = PackageAttributionInput::accepted_external(
                module.id,
                package_name,
                package_version,
                export_specifier,
            );
            if let Some((_package_name, Some(subpath))) = split_bare_specifier(export_specifier) {
                attribution = attribution.with_subpath(subpath);
            }
            report.attributions.push(attribution);
        }
        report.matches.push(PackageMatch {
            module_id: module.id,
            package_name: package_name.to_string(),
            package_version: package_version.to_string(),
            export_specifier: export_specifier.to_string(),
            source_path: format!("cascade:{export_specifier}"),
            normalized_source_hash: String::new(),
            strategy,
            function_signature_matches: covered_count,
            string_anchor_matches: 0,
            external_importable: can_externalize,
        });
    }
}

fn promote_structural_bag_ownership_matches(
    rows: &InputRows,
    structural_matches: &[PackageMatch],
    report: &mut VersionedPackageMatchReport,
) {
    let already_accepted = accepted_external_modules(rows, report);
    let mut matched_modules = report
        .matches
        .iter()
        .map(|package_match| package_match.module_id)
        .collect::<BTreeSet<_>>();
    let ownership_by_module = ownership_by_module(rows, report);

    for package_match in structural_matches {
        if package_match.external_importable
            || already_accepted.contains(&package_match.module_id)
            || matched_modules.contains(&package_match.module_id)
            || has_direct_neighborhood_package_contradiction(
                rows,
                package_match.module_id,
                package_match.package_name.as_str(),
                &ownership_by_module,
            )
        {
            continue;
        }
        matched_modules.insert(package_match.module_id);
        report.matches.push(package_match.clone());
    }
}

fn promote_exact_hint_ownership_matches(
    rows: &InputRows,
    package_sources: &[PackageSource],
    report: &mut VersionedPackageMatchReport,
) {
    let available_versions = package_sources
        .iter()
        .map(|source| {
            (
                source.package_name.as_str().to_string(),
                source.package_version.as_str().to_string(),
            )
        })
        .collect::<BTreeSet<_>>();
    if available_versions.is_empty() {
        return;
    }

    let already_accepted = accepted_external_modules(rows, report);
    let mut matched_modules = report
        .matches
        .iter()
        .map(|package_match| package_match.module_id)
        .collect::<BTreeSet<_>>();

    for module in &rows.modules {
        if module.kind != ModuleKind::Package
            || already_accepted.contains(&module.id)
            || matched_modules.contains(&module.id)
        {
            continue;
        }
        let Some(package_name) = module
            .package_name
            .as_deref()
            .map(str::trim)
            .filter(|package_name| !package_name.is_empty())
        else {
            continue;
        };
        let Some(package_version) = module
            .package_version
            .as_deref()
            .map(str::trim)
            .filter(|package_version| !package_version.is_empty())
            .filter(|package_version| Version::parse(package_version).is_ok())
        else {
            continue;
        };
        if !available_versions.contains(&(package_name.to_string(), package_version.to_string())) {
            continue;
        }
        let Some(slice) = rows.module_source_slice(module.id) else {
            continue;
        };
        let quality = package_module_source_quality(module, slice.source_file_path, slice.source);
        if quality == PackageModuleSourceQuality::Invalid {
            continue;
        }
        let external_specifier = (quality == PackageModuleSourceQuality::Trusted)
            .then(|| {
                exact_hint_root_external_specifier(package_sources, package_name, package_version)
            })
            .flatten();
        let external_importable = external_specifier.is_some();
        let export_specifier = external_specifier.unwrap_or_else(|| package_name.to_string());
        matched_modules.insert(module.id);
        if external_importable {
            report
                .attributions
                .push(PackageAttributionInput::accepted_external(
                    module.id,
                    package_name,
                    package_version,
                    export_specifier.as_str(),
                ));
        }
        report.matches.push(PackageMatch {
            module_id: module.id,
            package_name: package_name.to_string(),
            package_version: package_version.to_string(),
            export_specifier,
            source_path: format!(
                "exact-hint:{package_name}@{package_version}:quality={}:semantic_path={}",
                package_module_source_quality_label(quality),
                module.semantic_path,
            ),
            normalized_source_hash: String::new(),
            strategy: ModuleMatchStrategy::DependencyClosureOwnership,
            function_signature_matches: 0,
            string_anchor_matches: 0,
            external_importable,
        });
    }
}

fn exact_hint_root_external_specifier(
    package_sources: &[PackageSource],
    package_name: &str,
    package_version: &str,
) -> Option<String> {
    let specifiers = package_sources
        .iter()
        .filter(|source| {
            source.package_name == package_name
                && source.package_version == package_version
                && source.external_importable
                && !is_json_source_path(source.source_path.as_str())
                && source.export_specifier == package_name
        })
        .map(|source| source.export_specifier.clone())
        .collect::<BTreeSet<_>>();
    (specifiers.len() == 1).then(|| {
        specifiers
            .into_iter()
            .next()
            .expect("one root external specifier")
    })
}

fn promote_dependency_closure_ownership_matches(
    rows: &InputRows,
    report: &mut VersionedPackageMatchReport,
) {
    let already_accepted = accepted_external_modules(rows, report);
    let mut matched_modules = report
        .matches
        .iter()
        .map(|package_match| package_match.module_id)
        .collect::<BTreeSet<_>>();
    let mut ownership_by_module = ownership_by_module(rows, report);

    let mut round = 0usize;
    loop {
        round += 1;
        let mut promoted = Vec::<(PackageMatch, DependencyNeighborhoodEvidence)>::new();
        for module in &rows.modules {
            if module.kind != ModuleKind::Package
                || already_accepted.contains(&module.id)
                || matched_modules.contains(&module.id)
            {
                continue;
            }
            let Some(package_name) = module.package_name.as_deref() else {
                continue;
            };
            let Some(evidence) = dependency_neighborhood_ownership_evidence(
                rows,
                module,
                package_name,
                &ownership_by_module,
            ) else {
                continue;
            };
            promoted.push((
                PackageMatch {
                    module_id: module.id,
                    package_name: package_name.to_string(),
                    package_version: evidence.package_version.clone(),
                    export_specifier: package_name.to_string(),
                    source_path: dependency_neighborhood_source_path(
                        package_name,
                        &evidence,
                        round,
                    ),
                    normalized_source_hash: String::new(),
                    strategy: ModuleMatchStrategy::DependencyClosureOwnership,
                    function_signature_matches: evidence.same_package_owned_neighbors,
                    string_anchor_matches: evidence.owned_neighbors,
                    external_importable: false,
                },
                evidence,
            ));
        }
        if promoted.is_empty() {
            break;
        }
        for (package_match, evidence) in promoted {
            matched_modules.insert(package_match.module_id);
            ownership_by_module.insert(
                package_match.module_id,
                (
                    package_match.package_name.clone(),
                    evidence.package_version.clone(),
                ),
            );
            report.matches.push(package_match);
        }
    }
}

fn promote_dependency_cluster_ownership_matches(
    rows: &InputRows,
    report: &mut VersionedPackageMatchReport,
) {
    let already_accepted = accepted_external_modules(rows, report);
    let mut matched_modules = report
        .matches
        .iter()
        .map(|package_match| package_match.module_id)
        .collect::<BTreeSet<_>>();
    let mut ownership_by_module = ownership_by_module(rows, report);
    let modules_by_id = rows
        .modules
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();

    for component in package_dependency_components(rows) {
        let component_modules = component
            .iter()
            .filter_map(|module_id| modules_by_id.get(module_id).copied())
            .collect::<Vec<_>>();
        let package_named_count = component_modules
            .iter()
            .filter(|module| module.package_name.is_some())
            .count();
        if package_named_count < 4 {
            continue;
        }
        let component_owned_total = component
            .iter()
            .filter(|module_id| ownership_by_module.contains_key(module_id))
            .count();
        if component_owned_total < 3 {
            continue;
        }
        let mut hint_counts = BTreeMap::<String, usize>::new();
        let mut seed_counts = BTreeMap::<String, BTreeMap<String, usize>>::new();
        for module in &component_modules {
            if let Some(package_name) = module.package_name.as_deref() {
                *hint_counts.entry(package_name.to_string()).or_default() += 1;
            }
            if let Some((package_name, package_version)) = ownership_by_module.get(&module.id) {
                *seed_counts
                    .entry(package_name.clone())
                    .or_default()
                    .entry(package_version.clone())
                    .or_default() += 1;
            }
        }

        for (package_name, hint_count) in hint_counts {
            let Some(version_counts) = seed_counts.get(&package_name) else {
                continue;
            };
            let same_package_seed_count = version_counts.values().sum::<usize>();
            if same_package_seed_count < 3
                || same_package_seed_count * 100 < component_owned_total * 70
                || same_package_seed_count * 100 < hint_count * 10
            {
                continue;
            }
            let Some((package_version, version_seed_count)) = version_counts
                .iter()
                .max_by(|left, right| left.1.cmp(right.1).then_with(|| right.0.cmp(left.0)))
            else {
                continue;
            };
            if *version_seed_count * 100 < same_package_seed_count * 70 {
                continue;
            }

            let target_modules = component_modules
                .iter()
                .copied()
                .filter(|module| {
                    module.package_name.as_deref() == Some(package_name.as_str())
                        && !already_accepted.contains(&module.id)
                        && !matched_modules.contains(&module.id)
                        && module.package_version.as_deref().is_none_or(|expected| {
                            expected.trim().is_empty() || expected.trim() == package_version
                        })
                        && !has_direct_neighborhood_package_contradiction(
                            rows,
                            module.id,
                            package_name.as_str(),
                            &ownership_by_module,
                        )
                })
                .collect::<Vec<_>>();
            if target_modules.is_empty() {
                continue;
            }

            for module in target_modules {
                matched_modules.insert(module.id);
                ownership_by_module
                    .insert(module.id, (package_name.clone(), package_version.clone()));
                report.matches.push(PackageMatch {
                    module_id: module.id,
                    package_name: package_name.clone(),
                    package_version: package_version.clone(),
                    export_specifier: package_name.clone(),
                    source_path: format!(
                        "dependency-cluster:{package_name}@{package_version}:owned_seeds={same_package_seed_count}/{component_owned_total}:version_seeds={version_seed_count}:hinted={hint_count}/{package_named_count}:component_size={}",
                        component.len(),
                    ),
                    normalized_source_hash: String::new(),
                    strategy: ModuleMatchStrategy::DependencyClosureOwnership,
                    function_signature_matches: same_package_seed_count,
                    string_anchor_matches: hint_count,
                    external_importable: false,
                });
            }
        }
    }
}

fn promote_package_file_graph_ownership_matches(
    rows: &InputRows,
    report: &mut VersionedPackageMatchReport,
) {
    let already_accepted = accepted_external_modules(rows, report);
    let mut matched_modules = report
        .matches
        .iter()
        .map(|package_match| package_match.module_id)
        .collect::<BTreeSet<_>>();
    let mut ownership_by_module = ownership_by_module(rows, report);
    let mut modules_by_file = BTreeMap::<u32, Vec<&ModuleInput>>::new();
    for module in &rows.modules {
        if module.kind != ModuleKind::Package || module.source_span.is_none() {
            continue;
        }
        let Some(source_file_id) = module.source_file_id else {
            continue;
        };
        modules_by_file
            .entry(source_file_id)
            .or_default()
            .push(module);
    }

    for (source_file_id, mut file_modules) in modules_by_file {
        file_modules.sort_by(|left, right| {
            module_file_order_key(left)
                .cmp(&module_file_order_key(right))
                .then_with(|| left.id.cmp(&right.id))
        });
        for run in package_file_graph_runs(file_modules.as_slice()) {
            promote_package_file_graph_run(
                rows,
                source_file_id,
                run.as_slice(),
                &already_accepted,
                &mut matched_modules,
                &mut ownership_by_module,
                report,
            );
        }
    }
}

fn module_file_order_key(module: &ModuleInput) -> (u32, u32) {
    module
        .source_span
        .map(|span| (span.byte_start, span.byte_end))
        .unwrap_or((u32::MAX, u32::MAX))
}

fn package_file_graph_runs<'a>(file_modules: &'a [&'a ModuleInput]) -> Vec<Vec<&'a ModuleInput>> {
    let mut runs = Vec::new();
    let mut current = Vec::<&ModuleInput>::new();
    let mut current_package_name: Option<&str> = None;
    for module in file_modules.iter().copied() {
        let module_package_name = module.package_name.as_deref();
        if !current.is_empty() && module_package_name != current_package_name {
            runs.push(std::mem::take(&mut current));
        }
        current_package_name = module_package_name;
        current.push(module);
    }
    if !current.is_empty() {
        runs.push(current);
    }
    runs
}

fn promote_package_file_graph_run(
    rows: &InputRows,
    source_file_id: u32,
    run: &[&ModuleInput],
    already_accepted: &BTreeSet<ModuleId>,
    matched_modules: &mut BTreeSet<ModuleId>,
    ownership_by_module: &mut BTreeMap<ModuleId, (String, String)>,
    report: &mut VersionedPackageMatchReport,
) {
    if run.len() < 3 {
        return;
    }
    let Some(package_name) = run
        .first()
        .and_then(|module| module.package_name.as_deref())
        .filter(|package_name| !package_name.trim().is_empty())
    else {
        return;
    };
    let mut owned_seed_count = 0usize;
    let mut same_package_versions = BTreeMap::<String, usize>::new();
    for module in run {
        let Some((owned_package_name, owned_package_version)) = ownership_by_module.get(&module.id)
        else {
            continue;
        };
        owned_seed_count += 1;
        if owned_package_name == package_name {
            *same_package_versions
                .entry(owned_package_version.clone())
                .or_default() += 1;
        }
    }
    let same_package_seed_count = same_package_versions.values().sum::<usize>();
    if owned_seed_count == 0
        || same_package_seed_count < 2
        || same_package_seed_count * 100 < owned_seed_count * 70
    {
        return;
    }
    let Some((package_version, version_seed_count)) = same_package_versions
        .iter()
        .max_by(|left, right| left.1.cmp(right.1).then_with(|| right.0.cmp(left.0)))
    else {
        return;
    };
    if *version_seed_count * 100 < same_package_seed_count * 70 {
        return;
    }
    let Some((run_start, run_end)) = package_file_graph_run_span(run) else {
        return;
    };

    for module in run {
        if already_accepted.contains(&module.id) || matched_modules.contains(&module.id) {
            continue;
        }
        if module.package_version.as_deref().is_some_and(|expected| {
            let expected = expected.trim();
            !expected.is_empty() && expected != package_version
        }) {
            continue;
        }
        if !package_file_graph_module_has_usable_source(rows, module) {
            continue;
        }
        if has_direct_neighborhood_package_contradiction(
            rows,
            module.id,
            package_name,
            ownership_by_module,
        ) {
            continue;
        }
        matched_modules.insert(module.id);
        ownership_by_module.insert(
            module.id,
            (package_name.to_string(), package_version.clone()),
        );
        report.matches.push(PackageMatch {
            module_id: module.id,
            package_name: package_name.to_string(),
            package_version: package_version.clone(),
            export_specifier: package_name.to_string(),
            source_path: format!(
                "package-file-graph:{package_name}@{package_version}:file={source_file_id}:owned_seeds={same_package_seed_count}/{owned_seed_count}:version_seeds={version_seed_count}:run_size={}:span={run_start}..{run_end}",
                run.len(),
            ),
            normalized_source_hash: String::new(),
            strategy: ModuleMatchStrategy::DependencyClosureOwnership,
            function_signature_matches: same_package_seed_count,
            string_anchor_matches: run.len(),
            external_importable: false,
        });
    }
}

fn package_file_graph_run_span(run: &[&ModuleInput]) -> Option<(u32, u32)> {
    let start = run
        .iter()
        .filter_map(|module| module.source_span.map(|span| span.byte_start))
        .min()?;
    let end = run
        .iter()
        .filter_map(|module| module.source_span.map(|span| span.byte_end))
        .max()?;
    Some((start, end))
}

fn package_file_graph_module_has_usable_source(rows: &InputRows, module: &ModuleInput) -> bool {
    let Some(slice) = rows.module_source_slice(module.id) else {
        return false;
    };
    package_module_source_quality(module, slice.source_file_path, slice.source)
        != PackageModuleSourceQuality::Invalid
}

fn accepted_external_modules(
    rows: &InputRows,
    report: &VersionedPackageMatchReport,
) -> BTreeSet<ModuleId> {
    report
        .attributions
        .iter()
        .chain(rows.package_attributions.iter())
        .filter(|attribution| {
            attribution.status == PackageAttributionStatus::Accepted
                && attribution.emission_mode == PackageEmissionMode::ExternalImport
        })
        .map(|attribution| attribution.module_id)
        .collect()
}

fn package_dependency_components(rows: &InputRows) -> Vec<BTreeSet<ModuleId>> {
    let package_modules = rows
        .modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
        .map(|module| module.id)
        .collect::<BTreeSet<_>>();
    let mut adjacency = package_modules
        .iter()
        .map(|module_id| (*module_id, BTreeSet::new()))
        .collect::<BTreeMap<_, _>>();
    for dependency in &rows.dependencies {
        let from = dependency.from_module_id;
        let ModuleDependencyTarget::Module(to) = dependency.target else {
            continue;
        };
        if !package_modules.contains(&from) || !package_modules.contains(&to) {
            continue;
        }
        adjacency.entry(from).or_default().insert(to);
        adjacency.entry(to).or_default().insert(from);
    }

    let mut seen = BTreeSet::new();
    let mut components = Vec::new();
    for module_id in package_modules {
        if seen.contains(&module_id) {
            continue;
        }
        let mut stack = vec![module_id];
        let mut component = BTreeSet::new();
        while let Some(current) = stack.pop() {
            if !seen.insert(current) {
                continue;
            }
            component.insert(current);
            if let Some(neighbors) = adjacency.get(&current) {
                for neighbor in neighbors {
                    if !seen.contains(neighbor) {
                        stack.push(*neighbor);
                    }
                }
            }
        }
        components.push(component);
    }
    components
}

fn has_direct_neighborhood_package_contradiction(
    rows: &InputRows,
    module_id: ModuleId,
    package_name: &str,
    ownership_by_module: &BTreeMap<ModuleId, (String, String)>,
) -> bool {
    let (same, owned) = directional_owned_neighbor_counts(
        direct_module_neighborhood(rows, module_id)
            .into_iter()
            .collect(),
        package_name,
        ownership_by_module,
    );
    owned > 0 && same * 100 < owned * 50
}

#[derive(Debug, Clone)]
struct DependencyNeighborhoodEvidence {
    package_version: String,
    same_package_owned_neighbors: usize,
    owned_neighbors: usize,
    same_version_owned_neighbors: usize,
    same_outgoing_neighbors: usize,
    owned_outgoing_neighbors: usize,
    same_incoming_neighbors: usize,
    owned_incoming_neighbors: usize,
}

fn dependency_neighborhood_ownership_evidence(
    rows: &InputRows,
    module: &ModuleInput,
    package_name: &str,
    ownership_by_module: &BTreeMap<ModuleId, (String, String)>,
) -> Option<DependencyNeighborhoodEvidence> {
    let mut same_package_by_version = BTreeMap::<String, usize>::new();
    let mut owned_neighbors = 0usize;
    for neighbor_id in direct_module_neighborhood(rows, module.id) {
        let Some((neighbor_package, neighbor_version)) = ownership_by_module.get(&neighbor_id)
        else {
            continue;
        };
        owned_neighbors += 1;
        if neighbor_package == package_name {
            *same_package_by_version
                .entry(neighbor_version.clone())
                .or_default() += 1;
        }
    }
    let same_package_owned_neighbors = same_package_by_version.values().sum::<usize>();
    if same_package_owned_neighbors < 2
        || owned_neighbors == 0
        || same_package_owned_neighbors * 100 < owned_neighbors * 70
    {
        return None;
    }
    let (package_version, same_version_owned_neighbors) = same_package_by_version
        .iter()
        .max_by(|left, right| left.1.cmp(right.1).then_with(|| right.0.cmp(left.0)))?;
    if let Some(expected_version) = module
        .package_version
        .as_deref()
        .map(str::trim)
        .filter(|version| !version.is_empty())
        && expected_version != package_version
    {
        return None;
    }
    if *same_version_owned_neighbors * 100 < same_package_owned_neighbors * 70 {
        return None;
    }

    let (same_outgoing_neighbors, owned_outgoing_neighbors) = directional_owned_neighbor_counts(
        direct_module_dependencies(rows, module.id),
        package_name,
        ownership_by_module,
    );
    let (same_incoming_neighbors, owned_incoming_neighbors) = directional_owned_neighbor_counts(
        direct_module_dependents(rows, module.id),
        package_name,
        ownership_by_module,
    );

    Some(DependencyNeighborhoodEvidence {
        package_version: package_version.clone(),
        same_package_owned_neighbors,
        owned_neighbors,
        same_version_owned_neighbors: *same_version_owned_neighbors,
        same_outgoing_neighbors,
        owned_outgoing_neighbors,
        same_incoming_neighbors,
        owned_incoming_neighbors,
    })
}

fn directional_owned_neighbor_counts(
    neighbor_ids: Vec<ModuleId>,
    package_name: &str,
    ownership_by_module: &BTreeMap<ModuleId, (String, String)>,
) -> (usize, usize) {
    let mut seen = BTreeSet::new();
    let mut same = 0usize;
    let mut owned = 0usize;
    for neighbor_id in neighbor_ids {
        if !seen.insert(neighbor_id) {
            continue;
        }
        let Some((neighbor_package, _)) = ownership_by_module.get(&neighbor_id) else {
            continue;
        };
        owned += 1;
        if neighbor_package == package_name {
            same += 1;
        }
    }
    (same, owned)
}

fn dependency_neighborhood_source_path(
    package_name: &str,
    evidence: &DependencyNeighborhoodEvidence,
    round: usize,
) -> String {
    format!(
        "dependency-closure:{}@{}:owned_neighbors={}/{}:version_neighbors={}:out={}/{}:in={}/{}:round={}",
        package_name,
        evidence.package_version,
        evidence.same_package_owned_neighbors,
        evidence.owned_neighbors,
        evidence.same_version_owned_neighbors,
        evidence.same_outgoing_neighbors,
        evidence.owned_outgoing_neighbors,
        evidence.same_incoming_neighbors,
        evidence.owned_incoming_neighbors,
        round,
    )
}

fn direct_module_neighborhood(rows: &InputRows, module_id: ModuleId) -> BTreeSet<ModuleId> {
    direct_module_dependencies(rows, module_id)
        .into_iter()
        .chain(direct_module_dependents(rows, module_id))
        .collect()
}

fn accept_partial_cascade_coverage(
    total_functions: usize,
    covered_functions: usize,
    ownership_spans: usize,
    runner_up_functions: usize,
) -> bool {
    if total_functions < 3 || covered_functions < 2 {
        return false;
    }
    if covered_functions * 100 < total_functions * 65 {
        return false;
    }
    if ownership_spans == 0 || covered_functions * 100 < ownership_spans * 80 {
        return false;
    }
    runner_up_functions == 0 || covered_functions >= runner_up_functions.saturating_mul(3)
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
    /// strategy that proved this module/source match.
    pub strategy: ModuleMatchStrategy,
    /// overlapping function signatures.
    pub function_signature_matches: usize,
    /// overlapping string anchors.
    pub string_anchor_matches: usize,
    /// Whether this match was eligible for external import emission.
    pub external_importable: bool,
}

impl PackageMatch {
    fn from_module_match(module_match: &ModulePackageMatch) -> Self {
        Self {
            module_id: module_match.module_id,
            package_name: module_match.package_name.clone(),
            package_version: module_match.package_version.clone(),
            export_specifier: module_match.export_specifier.clone(),
            source_path: module_match.source_path.clone(),
            normalized_source_hash: module_match.normalized_source_hash.clone(),
            strategy: module_match.strategy,
            function_signature_matches: module_match.function_signature_matches,
            string_anchor_matches: module_match.string_anchor_matches,
            external_importable: module_match.external_importable,
        }
    }
}

fn has_accepted_attribution(rows: &InputRows, module_id: ModuleId) -> bool {
    rows.package_attributions.iter().any(|attribution| {
        attribution.module_id == module_id
            && attribution.status == PackageAttributionStatus::Accepted
            && attribution.emission_mode == PackageEmissionMode::ExternalImport
    })
}

fn has_accepted_surface(rows: &InputRows, specifier: &str) -> bool {
    rows.package_surfaces.iter().any(|surface| {
        surface.status == PackageAttributionStatus::Accepted
            && surface.export_specifier.as_str() == specifier
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

/// Returns npm package names referenced by source-backed `import`, `export from`,
/// `require()`, or dynamic `import()` sites in the original source files.
#[must_use]
pub fn package_import_names_from_sources(rows: &InputRows) -> BTreeSet<String> {
    package_import_sites_from_sources(rows)
        .into_iter()
        .map(|site| site.package_name)
        .collect()
}

/// Extracts source-backed bare package import/require sites from whole source
/// files rather than from package-module rows. This is the path used for
/// packages such as `ws`/`undici` that appear as runtime dependencies but whose
/// implementation is not bundled as a module.
#[must_use]
pub fn package_import_sites_from_sources(rows: &InputRows) -> BTreeSet<PackageImportSite> {
    let mut sites = BTreeSet::new();
    for source_file in &rows.source_files {
        let Some(source) = source_file.source.as_deref() else {
            continue;
        };
        sites.extend(package_import_sites_from_source_file(
            source_file.id,
            source_file.path.as_str(),
            source,
        ));
    }
    sites
}

fn resolve_source_package_surfaces(
    rows: &InputRows,
    current_attributions: &[PackageAttributionInput],
    package_sources: &[PackageSource],
    package_filter: Option<&BTreeSet<String>>,
) -> Vec<PackageSurfaceInput> {
    let mut sites_by_specifier = BTreeMap::<(String, String), BTreeSet<String>>::new();
    for site in package_import_sites_from_sources(rows) {
        if let Some(package_filter) = package_filter
            && !package_filter.contains(site.package_name.as_str())
        {
            continue;
        }
        if has_accepted_surface(rows, site.specifier.as_str()) {
            continue;
        }
        sites_by_specifier
            .entry((site.package_name, site.specifier))
            .or_default()
            .insert(site.source_file_path);
    }

    let mut surfaces = Vec::new();
    for ((package_name, specifier), source_paths) in sites_by_specifier {
        let (package_version, evidence_kind) = external_package_version(
            rows,
            current_attributions,
            package_sources,
            package_name.as_str(),
        );
        let evidence = source_surface_evidence(
            package_name.as_str(),
            package_version.as_str(),
            specifier.as_str(),
            evidence_kind,
            &source_paths,
        );
        surfaces.push(
            PackageSurfaceInput::accepted_external(package_name, package_version, specifier)
                .with_evidence(evidence),
        );
    }
    surfaces
}

fn package_import_sites_from_source_file(
    source_file_id: u32,
    source_file_path: &str,
    source: &str,
) -> BTreeSet<PackageImportSite> {
    let allocator = Allocator::default();
    for source_type in
        source_type_candidates(Some(Path::new(source_file_path)), ParseGoal::TypeScript)
    {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if parsed.errors.is_empty() && !parsed.panicked {
            let mut visitor = SourcePackageImportVisitor::default();
            visitor.visit_program(&parsed.program);
            return visitor
                .specifiers
                .into_iter()
                .map(|(package_name, specifier)| PackageImportSite {
                    source_file_id,
                    source_file_path: source_file_path.to_string(),
                    package_name,
                    specifier,
                })
                .collect();
        }
    }
    BTreeSet::new()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SurfaceVersionEvidenceKind {
    PackageModule,
    AcceptedAttribution,
    CachedPackageSource,
    SourceImportWildcard,
}

impl SurfaceVersionEvidenceKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::PackageModule => "package_module_version",
            Self::AcceptedAttribution => "accepted_attribution_version",
            Self::CachedPackageSource => "cached_package_source_version",
            Self::SourceImportWildcard => "source_import_without_unique_version",
        }
    }
}

fn external_package_version(
    rows: &InputRows,
    current_attributions: &[PackageAttributionInput],
    package_sources: &[PackageSource],
    package_name: &str,
) -> (String, SurfaceVersionEvidenceKind) {
    let module_versions = rows
        .modules
        .iter()
        .filter(|module| {
            module.kind == ModuleKind::Package
                && module.package_name.as_deref() == Some(package_name)
        })
        .filter_map(|module| module.package_version.clone())
        .collect::<BTreeSet<_>>();
    if let Some(version) = unique_version(module_versions) {
        return (version, SurfaceVersionEvidenceKind::PackageModule);
    }

    let attribution_versions = rows
        .package_attributions
        .iter()
        .chain(current_attributions.iter())
        .filter(|attribution| {
            attribution.package_name == package_name
                && attribution.status == PackageAttributionStatus::Accepted
                && attribution.emission_mode == PackageEmissionMode::ExternalImport
        })
        .filter_map(|attribution| attribution.package_version.clone())
        .collect::<BTreeSet<_>>();
    if let Some(version) = unique_version(attribution_versions) {
        return (version, SurfaceVersionEvidenceKind::AcceptedAttribution);
    }

    let cached_versions = package_sources
        .iter()
        .filter(|source| source.package_name == package_name)
        .map(|source| source.package_version.clone())
        .collect::<BTreeSet<_>>();
    if let Some(version) = unique_version(cached_versions) {
        return (version, SurfaceVersionEvidenceKind::CachedPackageSource);
    }

    (
        "*".to_string(),
        SurfaceVersionEvidenceKind::SourceImportWildcard,
    )
}

fn unique_version(versions: BTreeSet<String>) -> Option<String> {
    if versions.len() == 1 {
        versions.into_iter().next()
    } else {
        None
    }
}

fn source_surface_evidence(
    package_name: &str,
    package_version: &str,
    export_specifier: &str,
    evidence_kind: SurfaceVersionEvidenceKind,
    source_paths: &BTreeSet<String>,
) -> String {
    serde_json::json!({
        "matcher": "source_package_import_surface",
        "package_name": package_name,
        "package_version": package_version,
        "export_specifier": export_specifier,
        "version_evidence": evidence_kind.as_str(),
        "source_paths": source_paths.iter().collect::<Vec<_>>(),
    })
    .to_string()
}

#[derive(Debug, Default)]
struct SourcePackageImportVisitor {
    specifiers: BTreeSet<(String, String)>,
}

impl<'a> Visit<'a> for SourcePackageImportVisitor {
    fn visit_import_declaration(&mut self, it: &ImportDeclaration<'a>) {
        self.record_specifier(it.source.value.as_str());
    }

    fn visit_export_named_declaration(&mut self, it: &ExportNamedDeclaration<'a>) {
        if let Some(source) = &it.source {
            self.record_specifier(source.value.as_str());
        }
        walk_export_named_declaration(self, it);
    }

    fn visit_export_all_declaration(&mut self, it: &ExportAllDeclaration<'a>) {
        self.record_specifier(it.source.value.as_str());
        walk_export_all_declaration(self, it);
    }

    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        if let Expression::Identifier(identifier) = &it.callee
            && identifier.name.as_str() == "require"
            && let Some(specifier) = it.arguments.first().and_then(argument_string_literal)
        {
            self.record_specifier(specifier);
        }
        walk_call_expression(self, it);
    }

    fn visit_import_expression(&mut self, it: &ImportExpression<'a>) {
        if let Some(specifier) = expression_string_literal(&it.source) {
            self.record_specifier(specifier);
        }
        walk_import_expression(self, it);
    }
}

impl SourcePackageImportVisitor {
    fn record_specifier(&mut self, specifier: &str) {
        if is_node_builtin(specifier) {
            return;
        }
        let Some((package_name, _subpath)) = split_bare_specifier(specifier) else {
            return;
        };
        if !is_valid_package_name(package_name.as_str()) {
            return;
        }
        self.specifiers
            .insert((package_name, specifier.to_string()));
    }
}

fn argument_string_literal<'a>(argument: &'a Argument<'a>) -> Option<&'a str> {
    match argument {
        Argument::StringLiteral(literal) => Some(literal.value.as_str()),
        _ => None,
    }
}

fn expression_string_literal<'a>(expression: &'a Expression<'a>) -> Option<&'a str> {
    match expression {
        Expression::StringLiteral(literal) => Some(literal.value.as_str()),
        _ => None,
    }
}

#[derive(Debug)]
struct PackageVersionIndex<'a> {
    packages: BTreeMap<String, Vec<PackageVersionCandidate<'a>>>,
}

impl<'a> PackageVersionIndex<'a> {
    fn build(package_sources: &'a [PackageSource], audit: &mut AuditReport) -> Self {
        let mut by_version = BTreeMap::<(String, String), Vec<PackageSourceFingerprint<'a>>>::new();
        for source in package_sources {
            if !source.is_within_fingerprint_budget() {
                continue;
            }
            match package_source_fingerprint(source) {
                Ok(fingerprint) => {
                    by_version
                        .entry((source.package_name.clone(), source.package_version.clone()))
                        .or_default()
                        .push(fingerprint);
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

        let mut packages = BTreeMap::<String, Vec<PackageVersionCandidate<'a>>>::new();
        for ((package_name, package_version), mut sources) in by_version {
            sources.sort_by(|left, right| {
                left.normalized_source_hash
                    .cmp(&right.normalized_source_hash)
                    .then_with(|| {
                        right
                            .source
                            .external_importable
                            .cmp(&left.source.external_importable)
                    })
                    .then_with(|| left.source.source_path.cmp(&right.source.source_path))
                    .then_with(|| {
                        left.source
                            .export_specifier
                            .cmp(&right.source.export_specifier)
                    })
            });
            packages
                .entry(package_name.clone())
                .or_default()
                .push(PackageVersionCandidate {
                    package_name,
                    package_version,
                    sources,
                });
        }

        for versions in packages.values_mut() {
            versions.sort_by(|left, right| {
                compare_versions(
                    left.package_version.as_str(),
                    right.package_version.as_str(),
                )
            });
        }

        Self { packages }
    }

    fn has_package_version(&self, package_name: &str, package_version: &str) -> bool {
        self.version_candidate(package_name, package_version)
            .is_some()
    }

    fn version_candidate(
        &self,
        package_name: &str,
        package_version: &str,
    ) -> Option<&PackageVersionCandidate<'a>> {
        self.packages
            .get(package_name)?
            .iter()
            .find(|candidate| candidate.package_version == package_version)
    }

    fn match_exact_version_for_package(
        &self,
        package_name: &str,
        package_version: &str,
        module_fingerprints: &[ModuleMatchFingerprint],
        config: &VersionedPackageMatcherConfig,
    ) -> BestVersionMatch {
        let Some(version) = self.version_candidate(package_name, package_version) else {
            return BestVersionMatch::NoMatch {
                package_name: package_name.to_string(),
                scores: Vec::new(),
            };
        };
        let mut scored = score_version(version, module_fingerprints, config);
        scored.score.binary_search_probes = 1;
        decision_from_single_scored_version(package_name, scored, config)
    }
}

#[derive(Debug)]
struct ScoredPackageVersion {
    score: VersionMatchScore,
    module_matches: Vec<ModulePackageMatch>,
}

fn decision_from_single_scored_version(
    package_name: &str,
    scored: ScoredPackageVersion,
    config: &VersionedPackageMatcherConfig,
) -> BestVersionMatch {
    let ScoredPackageVersion {
        score,
        module_matches,
    } = scored;
    if !score.has_evidence() {
        return BestVersionMatch::NoMatch {
            package_name: package_name.to_string(),
            scores: vec![score],
        };
    }
    if score.source_hash_matches == 0
        && (score.function_signature_matches < config.min_function_signature_matches
            || score.string_anchor_matches < config.min_string_anchor_matches)
    {
        return BestVersionMatch::InsufficientEvidence { score };
    }
    BestVersionMatch::Selected {
        score,
        module_matches,
    }
}

fn fingerprint_modules_for_package(
    rows: &InputRows,
    package_name: &str,
    audit: &mut AuditReport,
) -> Vec<ModuleMatchFingerprint> {
    let mut fingerprints = Vec::new();
    for module in rows.modules.iter().filter(|module| {
        module.kind == ModuleKind::Package
            && module.package_name.as_deref() == Some(package_name)
            && !has_accepted_attribution(rows, module.id)
    }) {
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

        match package_module_source_quality(module, slice.source_file_path, slice.source) {
            PackageModuleSourceQuality::Trusted => {}
            PackageModuleSourceQuality::Weak | PackageModuleSourceQuality::Invalid => continue,
        }

        match module_match_fingerprint(module, slice.source_file_path, slice.source) {
            Ok(fingerprint) => fingerprints.push(fingerprint),
            Err(message) => {
                audit.push(
                    AuditFinding::error(FindingCode::UnparseablePackageSource, message)
                        .with_module(module.id.0.to_string())
                        .with_binding(module.original_name.clone()),
                );
            }
        }
    }
    fingerprints
}

#[must_use]
pub fn package_module_source_quality(
    module: &ModuleInput,
    source_path: &str,
    source: &str,
) -> PackageModuleSourceQuality {
    if source.trim().is_empty() || !package_module_source_parses(source_path, source) {
        return PackageModuleSourceQuality::Invalid;
    }
    let Some(package_name) = module.package_name.as_deref() else {
        return PackageModuleSourceQuality::Trusted;
    };
    let hint_tokens = package_semantic_path_tokens(package_name, module.semantic_path.as_str());
    if hint_tokens.is_empty() {
        return PackageModuleSourceQuality::Trusted;
    }
    let normalized_source = normalize_hint_text(source);
    if hint_tokens
        .iter()
        .any(|token| normalized_source.contains(token.as_str()))
    {
        PackageModuleSourceQuality::Trusted
    } else {
        PackageModuleSourceQuality::Weak
    }
}

fn package_module_source_parses(source_path: &str, source: &str) -> bool {
    let allocator = Allocator::default();
    for source_type in source_type_candidates(Some(Path::new(source_path)), ParseGoal::TypeScript) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if parsed.errors.is_empty() && !parsed.panicked {
            return true;
        }
    }
    false
}

fn package_semantic_path_tokens(package_name: &str, semantic_path: &str) -> BTreeSet<String> {
    let clean = semantic_path
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/')
        .replace('\\', "/");
    let mut tokens = BTreeSet::new();
    for prefix in package_semantic_path_prefixes(package_name) {
        let Some(rest) = strip_package_prefix_from_semantic_path(clean.as_str(), prefix.as_str())
        else {
            continue;
        };
        for token in path_hint_tokens(strip_source_extension(rest)) {
            if is_strong_path_hint_token(token.as_str()) {
                tokens.insert(normalize_hint_text(token.as_str()));
            }
        }
    }
    tokens
}

fn strip_package_prefix_from_semantic_path<'a>(
    semantic_path: &'a str,
    prefix: &str,
) -> Option<&'a str> {
    if let Some(rest) = semantic_path.strip_prefix(format!("{prefix}/").as_str()) {
        return Some(rest);
    }
    for marker in [format!("/{prefix}/"), format!("-{prefix}/")] {
        if let Some(index) = semantic_path.find(marker.as_str()) {
            return semantic_path.get(index + marker.len()..);
        }
    }
    None
}

fn is_strong_path_hint_token(token: &str) -> bool {
    token.len() >= 4
        && !matches!(
            token,
            "node"
                | "node_modules"
                | "module"
                | "modules"
                | "internal"
                | "index"
                | "src"
                | "dist"
                | "lib"
                | "cjs"
                | "esm"
                | "mjs"
                | "umd"
                | "operators"
                | "observable"
        )
}

fn normalize_hint_text(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn module_match_fingerprint(
    module: &ModuleInput,
    path: &str,
    source: &str,
) -> Result<ModuleMatchFingerprint, String> {
    let source_fingerprint = fingerprint_source(path, source)?;
    Ok(ModuleMatchFingerprint {
        module_id: module.id,
        package_name: module.package_name.clone(),
        package_version: module.package_version.clone(),
        normalized_source_hash: source_fingerprint.normalized_source_hash,
        normalized_source_hashes: source_fingerprint.normalized_source_hashes,
        function_signature_hashes: source_fingerprint.function_signature_hashes,
        string_anchors: source_fingerprint.string_anchors,
    })
}

fn package_source_fingerprint<'a>(
    source: &'a PackageSource,
) -> Result<PackageSourceFingerprint<'a>, String> {
    let fingerprint = fingerprint_source(source.source_path.as_str(), source.source.as_str())?;
    Ok(PackageSourceFingerprint {
        source,
        normalized_source_hash: fingerprint.normalized_source_hash,
        normalized_source_hashes: fingerprint.normalized_source_hashes,
        function_signature_hashes: fingerprint.function_signature_hashes,
        string_anchors: fingerprint.string_anchors,
    })
}

#[derive(Debug)]
struct SourceFingerprint {
    normalized_source_hash: String,
    normalized_source_hashes: BTreeSet<String>,
    function_signature_hashes: BTreeSet<String>,
    string_anchors: BTreeSet<String>,
}

fn fingerprint_source(path: &str, source: &str) -> Result<SourceFingerprint, String> {
    let normalized = normalize_source(path, source)?;
    let ast = ast_fingerprint(path, normalized.as_str())?;
    let normalized_source_hash = stable_hash(normalized.as_bytes());
    let mut normalized_source_hashes = BTreeSet::new();
    normalized_source_hashes.insert(normalized_source_hash.clone());
    if normalized.len() <= MODULE_SOURCE_HASH_ALTERNATE_MAX_BYTES {
        for pass in stable_passes() {
            if !module_source_hash_alternate_pass_enabled(pass.id()) {
                continue;
            }
            let Ok(transformed) = apply_to_source(pass.as_ref(), normalized.as_str()) else {
                continue;
            };
            let Ok(renormalized) = normalize_source(path, transformed.as_str()) else {
                continue;
            };
            normalized_source_hashes.insert(stable_hash(renormalized.as_bytes()));
        }
    }
    Ok(SourceFingerprint {
        normalized_source_hash,
        normalized_source_hashes,
        function_signature_hashes: ast.function_signature_hashes,
        string_anchors: ast.string_anchors,
    })
}

#[derive(Debug, Default)]
struct AstFingerprint {
    function_signature_hashes: BTreeSet<String>,
    string_anchors: BTreeSet<String>,
}

fn ast_fingerprint(path: &str, normalized_source: &str) -> Result<AstFingerprint, String> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();
    for source_type in source_type_candidates(Some(Path::new(path)), ParseGoal::TypeScript) {
        let parsed = Parser::new(&allocator, normalized_source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if parsed.errors.is_empty() && !parsed.panicked {
            let mut visitor = FingerprintVisitor {
                source: normalized_source,
                fingerprint: AstFingerprint::default(),
            };
            visitor.visit_program(&parsed.program);
            return Ok(visitor.fingerprint);
        }

        errors.push(ParseError {
            source_type: format!("{source_type:?}"),
            diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
        });
    }

    Err(parse_error_message(
        &JsError::ParseFailed(errors),
        "source could not be parsed",
    ))
}

struct FingerprintVisitor<'s> {
    source: &'s str,
    fingerprint: AstFingerprint,
}

impl<'a> Visit<'a> for FingerprintVisitor<'_> {
    fn enter_node(&mut self, kind: AstKind<'a>) {
        match kind {
            AstKind::Function(function) => self.record_function(
                "function",
                function.r#async,
                function.generator,
                function.params.items.len(),
                function.span.start,
                function.span.end,
            ),
            AstKind::ArrowFunctionExpression(arrow) => self.record_arrow_function(arrow),
            AstKind::StringLiteral(literal) => self.record_string_anchor(literal.value.as_str()),
            _ => {}
        }
    }

    fn visit_template_element(&mut self, it: &TemplateElement<'a>) {
        if let Some(cooked) = &it.value.cooked {
            self.record_string_anchor(cooked.as_str());
        } else {
            self.record_string_anchor(it.value.raw.as_str());
        }
        walk_template_element(self, it);
    }
}

impl FingerprintVisitor<'_> {
    fn record_arrow_function(&mut self, arrow: &ArrowFunctionExpression<'_>) {
        self.record_function(
            "arrow",
            arrow.r#async,
            false,
            arrow.params.items.len(),
            arrow.span.start,
            arrow.span.end,
        );
    }

    fn record_function(
        &mut self,
        kind: &str,
        r#async: bool,
        generator: bool,
        parameter_count: usize,
        start: u32,
        end: u32,
    ) {
        let Some(source_slice) = self.source.get(start as usize..end as usize) else {
            return;
        };
        let mut hash = FNV_OFFSET_BASIS;
        update_stable_hash(&mut hash, kind.as_bytes());
        update_stable_hash(&mut hash, b"|async=");
        update_stable_hash(&mut hash, if r#async { b"1" } else { b"0" });
        update_stable_hash(&mut hash, b"|generator=");
        update_stable_hash(&mut hash, if generator { b"1" } else { b"0" });
        update_stable_hash(&mut hash, b"|params=");
        update_stable_hash(&mut hash, parameter_count.to_string().as_bytes());
        update_stable_hash(&mut hash, b"|source=");
        update_stable_hash(&mut hash, source_slice.as_bytes());
        self.fingerprint
            .function_signature_hashes
            .insert(format!("{hash:016x}"));
    }

    fn record_string_anchor(&mut self, value: &str) {
        let trimmed = value.trim();
        if trimmed.len() >= MIN_STRING_ANCHOR_LEN {
            self.fingerprint.string_anchors.insert(trimmed.to_string());
        }
    }
}

fn score_version<'a>(
    version: &PackageVersionCandidate<'a>,
    module_fingerprints: &[ModuleMatchFingerprint],
    config: &VersionedPackageMatcherConfig,
) -> ScoredPackageVersion {
    let mut module_matches = Vec::new();
    for module in module_fingerprints {
        if let Some(module_match) = best_source_match(version, module, config) {
            module_matches.push(module_match);
        }
    }

    let source_hash_matches = module_matches
        .iter()
        .filter(|module_match| module_match.strategy == ModuleMatchStrategy::NormalizedSourceHash)
        .count();
    let function_signature_matches = module_matches
        .iter()
        .map(|module_match| module_match.function_signature_matches)
        .sum::<usize>();
    let string_anchor_matches = module_matches
        .iter()
        .map(|module_match| module_match.string_anchor_matches)
        .sum::<usize>();
    let score = weighted_score(
        source_hash_matches,
        module_matches.len(),
        function_signature_matches,
        string_anchor_matches,
    );

    ScoredPackageVersion {
        score: VersionMatchScore {
            package_name: version.package_name.clone(),
            package_version: version.package_version.clone(),
            total_modules: module_fingerprints.len(),
            matched_modules: module_matches.len(),
            source_hash_matches,
            function_signature_matches,
            string_anchor_matches,
            score,
            binary_search_probes: 0,
        },
        module_matches,
    }
}

fn best_source_match(
    version: &PackageVersionCandidate<'_>,
    module: &ModuleMatchFingerprint,
    config: &VersionedPackageMatcherConfig,
) -> Option<ModulePackageMatch> {
    let exact_candidates = version
        .sources
        .iter()
        .filter(|source| {
            !source
                .normalized_source_hashes
                .is_disjoint(&module.normalized_source_hashes)
        })
        .collect::<Vec<_>>();
    if !exact_candidates.is_empty() {
        if let Some(selection) = disambiguate_exact_source_candidate(exact_candidates.as_slice()) {
            return Some(module_package_match(
                module,
                selection.source,
                ModuleMatchStrategy::NormalizedSourceHash,
                selection
                    .source
                    .function_signature_hashes
                    .intersection(&module.function_signature_hashes)
                    .count(),
                selection
                    .source
                    .string_anchors
                    .intersection(&module.string_anchors)
                    .count(),
                selection.external_importable,
            ));
        }
        return None;
    }

    let mut ranked = version
        .sources
        .iter()
        .filter_map(|source| {
            let function_signature_matches = source
                .function_signature_hashes
                .intersection(&module.function_signature_hashes)
                .count();
            let string_anchor_matches = source
                .string_anchors
                .intersection(&module.string_anchors)
                .count();
            if function_signature_matches >= config.min_function_signature_matches
                && string_anchor_matches >= config.min_string_anchor_matches
            {
                Some((source, function_signature_matches, string_anchor_matches))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| right.2.cmp(&left.2))
            .then_with(|| {
                right
                    .0
                    .source
                    .external_importable
                    .cmp(&left.0.source.external_importable)
            })
            .then_with(|| left.0.source.source_path.cmp(&right.0.source.source_path))
    });

    let Some(best) = ranked.first() else {
        return best_aggregate_match(version, module, config);
    };
    if ranked
        .get(1)
        .is_some_and(|next| next.1 == best.1 && next.2 == best.2)
    {
        return best_aggregate_match(version, module, config);
    }

    Some(module_package_match(
        module,
        best.0,
        ModuleMatchStrategy::FunctionSignatureAndStringAnchors,
        best.1,
        best.2,
        best.0.source.external_importable,
    ))
}

fn best_aggregate_match(
    version: &PackageVersionCandidate<'_>,
    module: &ModuleMatchFingerprint,
    config: &VersionedPackageMatcherConfig,
) -> Option<ModulePackageMatch> {
    let mut function_signature_hashes = BTreeSet::new();
    let mut string_anchors = BTreeSet::new();
    for source in &version.sources {
        function_signature_hashes.extend(source.function_signature_hashes.iter().cloned());
        string_anchors.extend(source.string_anchors.iter().cloned());
    }
    let function_signature_matches = function_signature_hashes
        .intersection(&module.function_signature_hashes)
        .count();
    let string_anchor_matches = string_anchors.intersection(&module.string_anchors).count();
    let min_function_matches = config.min_function_signature_matches.max(3);
    if function_signature_matches < min_function_matches
        || string_anchor_matches < config.min_string_anchor_matches
    {
        return None;
    }
    Some(module_package_aggregate_match(
        module,
        version,
        function_signature_matches,
        string_anchor_matches,
    ))
}

struct ExactCandidateSelection<'a> {
    source: &'a PackageSourceFingerprint<'a>,
    external_importable: bool,
}

fn disambiguate_exact_source_candidate<'a>(
    candidates: &[&'a PackageSourceFingerprint<'a>],
) -> Option<ExactCandidateSelection<'a>> {
    let unique_keys = candidates
        .iter()
        .map(|source| {
            (
                source.source.package_name.as_str(),
                source.source.package_version.as_str(),
                source.source.export_specifier.as_str(),
                source.source.source_path.as_str(),
            )
        })
        .collect::<BTreeSet<_>>();
    if unique_keys.len() == 1 {
        return candidates
            .first()
            .copied()
            .map(|source| ExactCandidateSelection {
                source,
                external_importable: source.source.external_importable,
            });
    }

    let package_versions = candidates
        .iter()
        .map(|source| {
            (
                source.source.package_name.as_str(),
                source.source.package_version.as_str(),
            )
        })
        .collect::<BTreeSet<_>>();
    if package_versions.len() == 1 {
        return candidates
            .first()
            .copied()
            .map(|source| ExactCandidateSelection {
                source,
                // Duplicate exact source bodies inside the same package
                // version prove ownership, but not a safe unique import
                // specifier.
                external_importable: false,
            });
    }

    None
}

fn module_package_match(
    module: &ModuleMatchFingerprint,
    source: &PackageSourceFingerprint<'_>,
    strategy: ModuleMatchStrategy,
    function_signature_matches: usize,
    string_anchor_matches: usize,
    external_importable: bool,
) -> ModulePackageMatch {
    let external_importable = external_importable
        && (!is_json_source_path(source.source.source_path.as_str())
            || strategy == ModuleMatchStrategy::NormalizedSourceHash);
    ModulePackageMatch {
        module_id: module.module_id,
        package_name: source.source.package_name.clone(),
        package_version: source.source.package_version.clone(),
        export_specifier: source.source.export_specifier.clone(),
        source_path: source.source.source_path.clone(),
        strategy,
        normalized_source_hash: source.normalized_source_hash.clone(),
        function_signature_matches,
        string_anchor_matches,
        external_importable,
    }
}

fn module_package_aggregate_match(
    module: &ModuleMatchFingerprint,
    version: &PackageVersionCandidate<'_>,
    function_signature_matches: usize,
    string_anchor_matches: usize,
) -> ModulePackageMatch {
    ModulePackageMatch {
        module_id: module.module_id,
        package_name: version.package_name.clone(),
        package_version: version.package_version.clone(),
        export_specifier: version.package_name.clone(),
        source_path: format!(
            "aggregate:{}@{}",
            version.package_name, version.package_version
        ),
        strategy: ModuleMatchStrategy::AggregateFunctionSignatureAndStringAnchors,
        normalized_source_hash: module.normalized_source_hash.clone(),
        function_signature_matches,
        string_anchor_matches,
        external_importable: false,
    }
}

fn accepted_attribution_from_match(module_match: &ModulePackageMatch) -> PackageAttributionInput {
    let mut attribution = PackageAttributionInput::accepted_external(
        module_match.module_id,
        module_match.package_name.as_str(),
        module_match.package_version.as_str(),
        module_match.export_specifier.as_str(),
    )
    .with_resolved_file(module_match.source_path.as_str());
    if let Some((_package_name, Some(subpath))) =
        split_bare_specifier(&module_match.export_specifier)
    {
        attribution = attribution.with_subpath(subpath);
    }
    attribution
}

fn compare_versions(left: &str, right: &str) -> Ordering {
    match (Version::parse(left), Version::parse(right)) {
        (Ok(left), Ok(right)) => left.cmp(&right),
        (Ok(_), Err(_)) => Ordering::Less,
        (Err(_), Ok(_)) => Ordering::Greater,
        (Err(_), Err(_)) => left.cmp(right),
    }
}

fn weighted_score(
    source_hash_matches: usize,
    matched_modules: usize,
    function_signature_matches: usize,
    string_anchor_matches: usize,
) -> u32 {
    (source_hash_matches as u32 * SOURCE_HASH_WEIGHT)
        + (matched_modules as u32 * MODULE_MATCH_WEIGHT)
        + (function_signature_matches as u32 * FUNCTION_SIGNATURE_WEIGHT)
        + (string_anchor_matches as u32 * STRING_ANCHOR_WEIGHT)
}

fn normalize_source(path: &str, source: &str) -> Result<String, String> {
    normalize_source_for_pipeline(source, Some(Path::new(path)))
        .map_err(|error| parse_error_message(&error, "source could not be parsed"))
}

const MIN_STRING_ANCHOR_LEN: usize = 3;
const SOURCE_HASH_WEIGHT: u32 = 10_000;
const MODULE_MATCH_WEIGHT: u32 = 1_000;
const FUNCTION_SIGNATURE_WEIGHT: u32 = 10;
const STRING_ANCHOR_WEIGHT: u32 = 1;
const MODULE_SOURCE_HASH_ALTERNATE_MAX_BYTES: usize = 64 * 1024;

fn module_source_hash_alternate_pass_enabled(pass: NormalizationPassId) -> bool {
    matches!(
        pass,
        NormalizationPassId::TsRuntimeErased
            | NormalizationPassId::JsxRuntimeNormalized
            | NormalizationPassId::BundlerWrapperUnwrapped
            | NormalizationPassId::HelperIdentityInlined
            | NormalizationPassId::ExportBoundaryNormalized
            | NormalizationPassId::CommonJsExportBoundaryNormalized
            | NormalizationPassId::BooleanUndefinedCanonicalised
            | NormalizationPassId::ComputedToStaticMember
            | NormalizationPassId::VoidZeroToUndefinedGuarded
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        BestVersionMatch, ModuleMatchStrategy, PACKAGE_SOURCE_FINGERPRINT_MAX_BYTES,
        PackageModuleSourceQuality, PackageSource, VersionedPackageMatcher,
        exact_hint_root_external_specifier, match_packages_with_pipeline, match_structural_bags,
        match_structural_bags_with_excluded_modules, package_import_names_from_sources,
        package_module_source_quality,
    };
    use reverts_input::{
        InputRows, ModuleDependencyInput, ModuleDependencyTarget, ModuleInput,
        PackageAttributionInput, ProjectInput, SourceFileInput, SourceSpan,
    };
    use reverts_ir::ModuleId;

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

    fn rows_with_package_source_at_version(source: &str, version: &str) -> InputRows {
        let mut rows = rows_with_package_source(source);
        rows.modules[0].package_version = Some(version.to_string());
        rows
    }

    #[test]
    fn pipeline_owns_empty_source_scope_without_cli_sidecar_report() {
        let rows = rows_with_package_source("export function add(a,b){return a+b}");

        let report = match_packages_with_pipeline(&rows, &[], None);

        assert!(report.package_report.audit.is_clean());
        assert!(report.package_report.matches.is_empty());
        assert!(report.package_report.attributions.is_empty());
        assert!(report.function_attributions.is_empty());
        assert_eq!(report.function_ownership_matches, 0);
    }

    #[test]
    fn package_module_source_quality_rejects_unparseable_span() {
        let module = ModuleInput::package(
            ModuleId(10),
            "m10",
            "modules/10-rxjs/operators/sample.ts",
            "rxjs",
            Some("7.8.2".to_string()),
        );

        let quality = package_module_source_quality(
            &module,
            "bundle.js",
            "lts.allowAbsoluteUrls !== void 0) K.allowAbsoluteU",
        );

        assert_eq!(quality, PackageModuleSourceQuality::Invalid);
    }

    #[test]
    fn package_module_source_quality_marks_parseable_missing_hint_as_weak() {
        let module = ModuleInput::package(
            ModuleId(10),
            "m10",
            "modules/10-rxjs/operators/sample.ts",
            "rxjs",
            Some("7.8.2".to_string()),
        );

        let quality =
            package_module_source_quality(&module, "bundle.js", "function unrelated(){return 1;}");

        assert_eq!(quality, PackageModuleSourceQuality::Weak);
    }

    #[test]
    fn package_module_source_quality_trusts_parseable_hint_token() {
        let module = ModuleInput::package(
            ModuleId(10),
            "m10",
            "modules/10-rxjs/operators/sample.ts",
            "rxjs",
            Some("7.8.2".to_string()),
        );

        let quality =
            package_module_source_quality(&module, "bundle.js", "function sample(){return 1;}");

        assert_eq!(quality, PackageModuleSourceQuality::Trusted);
    }

    #[test]
    fn versioned_matcher_skips_weak_path_hint_for_exact_matching() {
        let mut rows = rows_with_package_source("function unrelated(){return 1;}");
        rows.modules[0].semantic_path = "pkg/sample.ts".to_string();
        let package_sources = [PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/sample",
            "sample.js",
            "function unrelated(){return 1;}",
        )];

        let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

        assert!(report.audit.is_clean());
        assert!(report.matches.is_empty());
        assert!(report.attributions.is_empty());
    }

    #[test]
    fn exact_match_uses_normalized_source_before_accepting_attribution() {
        let rows =
            rows_with_package_source_at_version("export function add(a,b){return a+b}", "1.2.3");
        let package_sources = [PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/add",
            "add.js",
            "export function add(a, b) {\n  return a + b;\n}",
        )];

        let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

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
    fn versioned_matcher_uses_module_level_normalization_alternates() {
        let rows =
            rows_with_package_source_at_version("export function add(a,b){return a+b}", "1.2.3");
        let package_sources = [PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/add",
            "add.js",
            "function add(a, b) {\n  return a + b;\n}",
        )];

        let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

        assert!(report.audit.is_clean());
        assert_eq!(report.attributions.len(), 1);
        assert_eq!(
            report.matches[0].strategy,
            ModuleMatchStrategy::NormalizedSourceHash,
            "export-boundary normalization should produce a source hash match"
        );
        let selected = report
            .version_matches
            .iter()
            .find_map(|decision| match decision {
                BestVersionMatch::Selected { score, .. } => Some(score),
                _ => None,
            })
            .expect("exact version should be selected");
        assert_eq!(selected.source_hash_matches, 1);
    }

    #[test]
    fn versioned_matcher_matches_cjs_and_esm_export_boundaries() {
        let rows = rows_with_package_source_at_version(
            "function add(a,b){return a+b}\nexports.add = add;",
            "1.2.3",
        );
        let package_sources = [PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/add",
            "add.js",
            "export function add(a, b) {\n  return a + b;\n}",
        )];

        let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

        assert!(report.audit.is_clean());
        assert_eq!(report.attributions.len(), 1);
        assert_eq!(
            report.matches[0].strategy,
            ModuleMatchStrategy::NormalizedSourceHash,
            "CommonJS export footer stripping should recover a source hash match"
        );
    }

    #[test]
    fn versioned_matcher_matches_commonjs_define_property_reexport() {
        let rows = rows_with_package_source_at_version(
            r#"function add(a,b){return a+b}
Object.defineProperty(exports, "add", { enumerable: true, get: function () { return add; } });"#,
            "1.2.3",
        );
        let package_sources = [PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/add",
            "add.js",
            "export function add(a, b) {\n  return a + b;\n}",
        )];

        let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

        assert!(report.audit.is_clean());
        assert_eq!(report.attributions.len(), 1);
        assert_eq!(
            report.matches[0].strategy,
            ModuleMatchStrategy::NormalizedSourceHash
        );
    }

    #[test]
    fn versioned_matcher_externalizes_exact_json_source_with_resolved_file() {
        let source = "export default {\"aliceblue\":\"#f0f8ff\"};\n";
        let rows = rows_with_package_source_at_version(source, "1.0.0");
        let package_sources = [PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg",
            "pkg@1.0.0/data.json",
            source,
        )];

        let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

        assert!(report.audit.is_clean());
        assert_eq!(report.attributions.len(), 1);
        assert_eq!(
            report.matches[0].strategy,
            ModuleMatchStrategy::NormalizedSourceHash
        );
        assert!(report.matches[0].external_importable);
        assert_eq!(
            report.attributions[0].resolved_file.as_deref(),
            Some("pkg@1.0.0/data.json")
        );
    }

    #[test]
    fn exact_hint_promotion_does_not_externalize_json_without_source_match() {
        let package_sources = [PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg",
            "pkg@1.0.0/data.json",
            "export default {\"aliceblue\":\"#f0f8ff\"};\n",
        )];

        let specifier = exact_hint_root_external_specifier(&package_sources, "pkg", "1.0.0");

        assert_eq!(specifier, None);
    }

    #[test]
    fn versioned_matcher_uses_package_aggregate_ownership_when_sources_are_split() {
        let rows = rows_with_package_source_at_version(
            r#"
            function one(){return "alpha-anchor";}
            function two(){return "beta-anchor";}
            function three(){return "gamma-anchor";}
            "#,
            "1.2.3",
        );
        let package_sources = [
            PackageSource::external(
                "pkg",
                "1.2.3",
                "pkg/one",
                "one.js",
                r#"function one(){return "alpha-anchor";}"#,
            ),
            PackageSource::external(
                "pkg",
                "1.2.3",
                "pkg/two",
                "two.js",
                r#"function two(){return "beta-anchor";}"#,
            ),
            PackageSource::external(
                "pkg",
                "1.2.3",
                "pkg/three",
                "three.js",
                r#"function three(){return "gamma-anchor";}"#,
            ),
        ];

        let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

        assert!(report.audit.is_clean());
        assert!(
            report.attributions.is_empty(),
            "aggregate package ownership must not emit a single external import"
        );
        assert_eq!(report.matches.len(), 1);
        assert_eq!(
            report.matches[0].strategy,
            ModuleMatchStrategy::AggregateFunctionSignatureAndStringAnchors
        );
        assert!(!report.matches[0].external_importable);
        assert!(report.matches[0].function_signature_matches >= 3);
    }

    #[test]
    fn structural_bag_matches_weak_minified_aggregate_without_external_import() {
        let mut rows = rows_with_package_source(
            r#"
            function a(x){if(x){return true;}return false;}
            function b(y){if(y){return true;}return false;}
            "#,
        );
        rows.modules[0].semantic_path = "pkg/not-present-in-source.js".to_string();
        let package_sources = [
            PackageSource::external(
                "pkg",
                "1.2.3",
                "pkg/first",
                "first.js",
                "function first(value){if(value){return true;}return false;}",
            ),
            PackageSource::external(
                "pkg",
                "1.2.3",
                "pkg/second",
                "second.js",
                "function second(input){if(input){return true;}return false;}",
            ),
        ];

        let report = match_structural_bags(&rows, &package_sources, None);

        assert!(report.audit.is_clean());
        assert_eq!(report.matches.len(), 1);
        assert_eq!(
            report.matches[0].strategy,
            ModuleMatchStrategy::AggregateStructuralBagSimilarity
        );
        assert_eq!(report.matches[0].package_version, "1.2.3");
        assert!(!report.matches[0].external_importable);
        assert_eq!(report.matches[0].function_signature_matches, 2);
        assert!(
            report.matches[0].string_anchor_matches >= 2,
            "structural bag should count strong structural axes as evidence"
        );
    }

    #[test]
    fn structural_bag_rejects_equal_versions_without_version_hint() {
        let rows = rows_with_package_source(
            r#"
            function a(x){if(x){return true;}return false;}
            function b(y){if(y){return true;}return false;}
            "#,
        );
        let package_sources = [
            PackageSource::external(
                "pkg",
                "1.0.0",
                "pkg/first",
                "first.js",
                "function first(value){if(value){return true;}return false;}",
            ),
            PackageSource::external(
                "pkg",
                "2.0.0",
                "pkg/first",
                "first.js",
                "function first(value){if(value){return true;}return false;}",
            ),
        ];

        let report = match_structural_bags(&rows, &package_sources, None);

        assert!(report.audit.is_clean());
        assert!(
            report.matches.is_empty(),
            "equal structural evidence across versions must not infer a version"
        );
    }

    #[test]
    fn structural_bag_uses_exact_module_version_hint_for_equal_versions() {
        let mut rows = rows_with_package_source(
            r#"
            function a(x){if(x){return true;}return false;}
            function b(y){if(y){return true;}return false;}
            "#,
        );
        rows.modules[0].package_version = Some("1.0.0".to_string());
        let package_sources = [
            PackageSource::external(
                "pkg",
                "1.0.0",
                "pkg/first",
                "first.js",
                "function first(value){if(value){return true;}return false;}",
            ),
            PackageSource::external(
                "pkg",
                "2.0.0",
                "pkg/first",
                "first.js",
                "function first(value){if(value){return true;}return false;}",
            ),
        ];

        let report = match_structural_bags(&rows, &package_sources, None);

        assert!(report.audit.is_clean());
        assert_eq!(report.matches.len(), 1);
        assert_eq!(report.matches[0].package_version, "1.0.0");
        assert_eq!(
            report.matches[0].strategy,
            ModuleMatchStrategy::AggregateStructuralBagSimilarity
        );
    }

    #[test]
    fn structural_bag_skips_modules_already_matched_by_stronger_strategy() {
        let source = r#"
            function a(x){if(x){return true;}return false;}
            function b(y){if(y){return true;}return false;}
            "#;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle-one.js",
            Some(source.to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            2,
            "bundle-two.js",
            Some(source.to_string()),
        ));
        rows.modules.push(
            ModuleInput::package(ModuleId(10), "m10", "pkg/one.js", "pkg", None)
                .with_source_file(1),
        );
        rows.modules.push(
            ModuleInput::package(ModuleId(11), "m11", "pkg/two.js", "pkg", None)
                .with_source_file(2),
        );
        let package_sources = [
            PackageSource::external(
                "pkg",
                "1.2.3",
                "pkg/first",
                "first.js",
                "function first(value){if(value){return true;}return false;}",
            ),
            PackageSource::external(
                "pkg",
                "1.2.3",
                "pkg/second",
                "second.js",
                "function second(input){if(input){return true;}return false;}",
            ),
        ];

        let report = match_structural_bags_with_excluded_modules(
            &rows,
            &package_sources,
            None,
            &BTreeSet::from([ModuleId(10)]),
        );

        assert!(report.audit.is_clean());
        assert_eq!(report.matches.len(), 1);
        assert_eq!(report.matches[0].module_id, ModuleId(11));
    }

    #[test]
    fn pipeline_promotes_structural_bag_as_source_only_ownership() {
        let mut rows = rows_with_package_source(
            r#"
            function a(x){if(x){return true;}return false;}
            function b(y){if(y){return true;}return false;}
            "#,
        );
        rows.modules[0].semantic_path = "pkg/not-present-in-source.js".to_string();
        let package_sources = [
            PackageSource::external(
                "pkg",
                "1.2.3",
                "pkg/first",
                "first.js",
                "function first(value){if(value){return true;}return false;}",
            ),
            PackageSource::external(
                "pkg",
                "1.2.3",
                "pkg/second",
                "second.js",
                "function second(input){if(input){return true;}return false;}",
            ),
        ];

        let report = match_packages_with_pipeline(&rows, &package_sources, None);

        assert!(report.package_report.audit.is_clean());
        assert_eq!(
            report
                .package_report
                .matches
                .iter()
                .filter(|package_match| package_match.strategy
                    == ModuleMatchStrategy::AggregateStructuralBagSimilarity)
                .count(),
            1
        );
        let package_match = report
            .package_report
            .matches
            .iter()
            .find(|package_match| package_match.module_id == ModuleId(10))
            .expect("structural ownership should be promoted into package report");
        assert_eq!(
            package_match.strategy,
            ModuleMatchStrategy::AggregateStructuralBagSimilarity
        );
        assert!(
            package_match
                .source_path
                .contains("structural-bag:pkg@1.2.3")
        );
        assert!(!package_match.external_importable);
        assert!(
            report.package_report.attributions.is_empty(),
            "structural ownership alone must not emit an external import"
        );
    }

    #[test]
    fn pipeline_promotes_trusted_exact_hint_as_source_only_ownership() {
        let mut rows =
            rows_with_package_source_at_version("function sample(){return 42;}", "1.2.3");
        rows.modules[0].semantic_path = "pkg/sample.js".to_string();
        let package_sources = [PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/other",
            "other.js",
            "export const other = 'unrelated-package-source';",
        )];

        let report = match_packages_with_pipeline(&rows, &package_sources, None);

        assert!(report.package_report.audit.is_clean());
        assert_eq!(report.package_report.matches.len(), 1);
        assert_eq!(
            report.package_report.matches[0].strategy,
            ModuleMatchStrategy::DependencyClosureOwnership
        );
        assert!(
            report.package_report.matches[0]
                .source_path
                .contains("exact-hint:pkg@1.2.3:quality=trusted")
        );
        assert!(!report.package_report.matches[0].external_importable);
        assert!(report.package_report.attributions.is_empty());
    }

    #[test]
    fn pipeline_promotes_trusted_exact_hint_with_unique_root_surface_to_external_import() {
        let mut rows =
            rows_with_package_source_at_version("function sample(){return 42;}", "1.2.3");
        rows.modules[0].semantic_path = "pkg/index.js".to_string();
        let package_sources = [PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg",
            "pkg@1.2.3/dist/index.js",
            "export const unrelated = 'public-root-surface';",
        )];

        let report = match_packages_with_pipeline(&rows, &package_sources, None);

        assert!(report.package_report.audit.is_clean());
        assert_eq!(report.package_report.matches.len(), 1);
        assert_eq!(
            report.package_report.matches[0].strategy,
            ModuleMatchStrategy::DependencyClosureOwnership
        );
        assert!(
            report.package_report.matches[0]
                .source_path
                .contains("exact-hint:pkg@1.2.3:quality=trusted")
        );
        assert!(report.package_report.matches[0].external_importable);
        assert_eq!(
            report.package_report.matches[0].export_specifier.as_str(),
            "pkg"
        );
        assert_eq!(report.package_report.attributions.len(), 1);
        assert_eq!(
            report.package_report.attributions[0]
                .export_specifier
                .as_deref(),
            Some("pkg")
        );
    }

    #[test]
    fn pipeline_promotes_weak_exact_hint_as_source_only_ownership() {
        let mut rows =
            rows_with_package_source_at_version("function unrelated(){return 42;}", "1.2.3");
        rows.modules[0].semantic_path = "pkg/sample.js".to_string();
        let package_sources = [PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/other",
            "other.js",
            "export const other = 'unrelated-package-source';",
        )];

        let report = match_packages_with_pipeline(&rows, &package_sources, None);

        assert!(report.package_report.audit.is_clean());
        assert_eq!(report.package_report.matches.len(), 1);
        assert_eq!(
            report.package_report.matches[0].strategy,
            ModuleMatchStrategy::DependencyClosureOwnership
        );
        assert!(
            report.package_report.matches[0]
                .source_path
                .contains("exact-hint:pkg@1.2.3:quality=weak")
        );
        assert!(!report.package_report.matches[0].external_importable);
        assert!(report.package_report.attributions.is_empty());
    }

    #[test]
    fn pipeline_promotes_weak_exact_hint_despite_other_package_neighbor() {
        let mut rows =
            rows_with_package_source_at_version("function unrelated(){return 42;}", "1.2.3");
        rows.modules[0].semantic_path = "pkg/sample.js".to_string();
        rows.source_files.push(SourceFileInput::new(
            2,
            "other.js",
            Some("export const otherDep = 1;".to_string()),
        ));
        rows.modules.push(
            ModuleInput::package(
                ModuleId(11),
                "otherDep",
                "other/index.js",
                "other",
                Some("9.9.9".to_string()),
            )
            .with_source_file(2),
        );
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(10),
            target: ModuleDependencyTarget::Module(ModuleId(11)),
        });
        rows.package_attributions
            .push(PackageAttributionInput::accepted_external(
                ModuleId(11),
                "other",
                "9.9.9",
                "other",
            ));
        let package_sources = [PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/other",
            "other.js",
            "export const other = 'unrelated-package-source';",
        )];

        let report = match_packages_with_pipeline(&rows, &package_sources, None);

        assert!(report.package_report.audit.is_clean());
        let package_match = report
            .package_report
            .matches
            .iter()
            .find(|package_match| package_match.module_id == ModuleId(10))
            .expect("exact package hint should own the module even when imports point outside");
        assert!(
            package_match
                .source_path
                .contains("exact-hint:pkg@1.2.3:quality=weak"),
            "{}",
            package_match.source_path
        );
        assert!(!package_match.external_importable);
        assert!(
            report
                .package_report
                .attributions
                .iter()
                .all(|attribution| {
                    attribution.module_id != ModuleId(10)
                        || attribution.emission_mode
                            != reverts_input::PackageEmissionMode::ExternalImport
                }),
            "exact hints prove ownership only; they must not emit an external import"
        );
    }

    #[test]
    fn pipeline_promotes_exact_hint_without_fingerprinting_huge_package_source() {
        let mut rows =
            rows_with_package_source_at_version("function unrelated(){return 42;}", "1.2.3");
        rows.modules[0].semantic_path = "pkg/sample.js".to_string();
        let huge_source = format!(
            "export const oversized = \"{}\";",
            "x".repeat(PACKAGE_SOURCE_FINGERPRINT_MAX_BYTES + 1)
        );
        let package_sources = [PackageSource::source_only(
            "pkg",
            "1.2.3",
            "pkg/oversized",
            "oversized.js",
            huge_source,
        )];

        let report = match_packages_with_pipeline(&rows, &package_sources, None);

        assert!(report.package_report.audit.is_clean());
        assert_eq!(report.package_report.matches.len(), 1);
        assert_eq!(
            report.package_report.matches[0].strategy,
            ModuleMatchStrategy::DependencyClosureOwnership
        );
        assert!(
            report.package_report.matches[0]
                .source_path
                .contains("exact-hint:pkg@1.2.3")
        );
        assert!(report.package_report.audit.is_clean());
    }

    #[test]
    fn pipeline_does_not_promote_exact_hint_without_exact_version() {
        let mut rows = rows_with_package_source("function unrelated(){return 42;}");
        rows.modules[0].semantic_path = "pkg/sample.js".to_string();
        let package_sources = [PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/other",
            "other.js",
            "export const other = 'unrelated-package-source';",
        )];

        let report = match_packages_with_pipeline(&rows, &package_sources, None);

        assert!(report.package_report.audit.is_clean());
        assert!(report.package_report.matches.is_empty());
        assert!(report.package_report.attributions.is_empty());
    }

    #[test]
    fn source_only_package_source_matches_without_external_attribution() {
        let rows =
            rows_with_package_source_at_version("export function add(a,b){return a+b}", "1.2.3");
        let package_sources = [PackageSource::source_only(
            "pkg",
            "1.2.3",
            "pkg/lib/add.js",
            "lib/add.js",
            "export function add(a, b) {\n  return a + b;\n}",
        )];

        let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

        assert!(report.audit.is_clean());
        assert!(
            report.attributions.is_empty(),
            "source-only package sources must not be externalized"
        );
        assert_eq!(report.matches.len(), 1);
        assert_eq!(report.matches[0].package_name, "pkg");
        assert_eq!(report.matches[0].package_version, "1.2.3");
        assert_eq!(report.matches[0].source_path, "lib/add.js");
        assert!(!report.matches[0].external_importable);
        match &report.version_matches[0] {
            BestVersionMatch::Selected { module_matches, .. } => {
                assert_eq!(module_matches.len(), 1);
                assert!(!module_matches[0].external_importable);
            }
            other => panic!("expected source-only match to select a version, got {other:?}"),
        }
    }

    #[test]
    fn external_package_source_wins_over_duplicate_source_only_candidate() {
        let rows =
            rows_with_package_source_at_version("export function add(a,b){return a+b}", "1.2.3");
        let package_sources = [
            PackageSource::source_only(
                "pkg",
                "1.2.3",
                "pkg/add",
                "add.js",
                "export function add(a, b) {\n  return a + b;\n}",
            ),
            PackageSource::external(
                "pkg",
                "1.2.3",
                "pkg/add",
                "add.js",
                "export function add(a, b) {\n  return a + b;\n}",
            ),
        ];

        let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

        assert!(report.audit.is_clean());
        assert_eq!(report.attributions.len(), 1);
        assert_eq!(
            report.attributions[0].export_specifier.as_deref(),
            Some("pkg/add")
        );
        assert_eq!(report.matches.len(), 1);
        assert!(report.matches[0].external_importable);
    }

    #[test]
    fn duplicate_exact_sources_prove_ownership_without_external_import() {
        let rows =
            rows_with_package_source_at_version("export function add(a,b){return a+b}", "1.2.3");
        let package_sources = [
            PackageSource::external(
                "pkg",
                "1.2.3",
                "pkg/add",
                "add.js",
                "export function add(a, b) {\n  return a + b;\n}",
            ),
            PackageSource::external(
                "pkg",
                "1.2.3",
                "pkg/internal/add",
                "internal/add.js",
                "export function add(a, b) {\n  return a + b;\n}",
            ),
        ];

        let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

        assert!(report.audit.is_clean());
        assert!(
            report.attributions.is_empty(),
            "duplicate exact source bodies must not infer an import specifier"
        );
        assert_eq!(report.matches.len(), 1);
        assert_eq!(
            report.matches[0].strategy,
            ModuleMatchStrategy::NormalizedSourceHash
        );
        assert!(!report.matches[0].external_importable);
    }

    #[test]
    fn pipeline_promotes_dependency_neighborhood_from_incoming_edges() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "wrapper.js",
            Some("var wrap = E(() => { return {}; });".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            2,
            "one.js",
            Some("export function one(){return 'one-anchor';}".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            3,
            "two.js",
            Some("export function two(){return 'two-anchor';}".to_string()),
        ));
        rows.modules.push(
            ModuleInput::package(
                ModuleId(10),
                "wrapper",
                "pkg/incoming-wrapper.js",
                "pkg",
                None,
            )
            .with_source_file(1),
        );
        rows.modules.push(
            ModuleInput::package(ModuleId(11), "one", "pkg/one.js", "pkg", None)
                .with_source_file(2),
        );
        rows.modules.push(
            ModuleInput::package(ModuleId(12), "two", "pkg/two.js", "pkg", None)
                .with_source_file(3),
        );
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(11),
            target: ModuleDependencyTarget::Module(ModuleId(10)),
        });
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(12),
            target: ModuleDependencyTarget::Module(ModuleId(10)),
        });
        let package_sources = [
            PackageSource::external(
                "pkg",
                "1.2.3",
                "pkg/one",
                "one.js",
                "export function one(){return 'one-anchor';}",
            ),
            PackageSource::external(
                "pkg",
                "1.2.3",
                "pkg/two",
                "two.js",
                "export function two(){return 'two-anchor';}",
            ),
        ];

        let report = match_packages_with_pipeline(&rows, &package_sources, None);

        assert!(report.package_report.audit.is_clean());
        assert_eq!(report.package_report.matches.len(), 3);
        let wrapper_match = report
            .package_report
            .matches
            .iter()
            .find(|package_match| package_match.module_id == ModuleId(10))
            .expect("incoming wrapper should be promoted");
        assert_eq!(
            wrapper_match.strategy,
            ModuleMatchStrategy::DependencyClosureOwnership
        );
        assert!(wrapper_match.source_path.contains("owned_neighbors=2/2"));
        assert!(wrapper_match.source_path.contains("out=0/0"));
        assert!(wrapper_match.source_path.contains("in=2/2"));
        assert!(!wrapper_match.external_importable);
    }

    #[test]
    fn pipeline_iterates_dependency_neighborhood_ownership() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "first-wrapper.js",
            Some("var wrap = E(() => { one(); two(); });".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            2,
            "one.js",
            Some("export function one(){return 'one-anchor';}".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            3,
            "two.js",
            Some("export function two(){return 'two-anchor';}".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            4,
            "second-wrapper.js",
            Some("var secondWrap = E(() => { wrap(); two(); });".to_string()),
        ));
        rows.modules.push(
            ModuleInput::package(ModuleId(10), "wrapper", "pkg/first-wrapper.js", "pkg", None)
                .with_source_file(1),
        );
        rows.modules.push(
            ModuleInput::package(ModuleId(11), "one", "pkg/one.js", "pkg", None)
                .with_source_file(2),
        );
        rows.modules.push(
            ModuleInput::package(ModuleId(12), "two", "pkg/two.js", "pkg", None)
                .with_source_file(3),
        );
        rows.modules.push(
            ModuleInput::package(
                ModuleId(13),
                "secondWrapper",
                "pkg/second-wrapper.js",
                "pkg",
                None,
            )
            .with_source_file(4),
        );
        for (from, to) in [
            (ModuleId(10), ModuleId(11)),
            (ModuleId(10), ModuleId(12)),
            (ModuleId(13), ModuleId(10)),
            (ModuleId(13), ModuleId(12)),
        ] {
            rows.dependencies.push(ModuleDependencyInput {
                from_module_id: from,
                target: ModuleDependencyTarget::Module(to),
            });
        }
        let package_sources = [
            PackageSource::external(
                "pkg",
                "1.2.3",
                "pkg/one",
                "one.js",
                "export function one(){return 'one-anchor';}",
            ),
            PackageSource::external(
                "pkg",
                "1.2.3",
                "pkg/two",
                "two.js",
                "export function two(){return 'two-anchor';}",
            ),
        ];

        let report = match_packages_with_pipeline(&rows, &package_sources, None);

        assert!(report.package_report.audit.is_clean());
        assert_eq!(report.package_report.matches.len(), 4);
        let second_wrapper_match = report
            .package_report
            .matches
            .iter()
            .find(|package_match| package_match.module_id == ModuleId(13))
            .expect("second wrapper should be promoted in a later round");
        assert!(
            second_wrapper_match
                .source_path
                .contains("owned_neighbors=2/2")
        );
        assert!(second_wrapper_match.source_path.contains("round=2"));
    }

    #[test]
    fn pipeline_promotes_dependency_cluster_ownership() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        for (id, path, source) in [
            (
                1,
                "cluster-member.js",
                "var clusterMember = E(() => { one(); });",
            ),
            (2, "one.js", "export function one(){return 'one-anchor';}"),
            (3, "two.js", "export function two(){return 'two-anchor';}"),
            (
                4,
                "three.js",
                "export function three(){return 'three-anchor';}",
            ),
        ] {
            rows.source_files
                .push(SourceFileInput::new(id, path, Some(source.to_string())));
        }
        rows.modules.push(
            ModuleInput::package(
                ModuleId(10),
                "clusterMember",
                "pkg/cluster-member.js",
                "pkg",
                None,
            )
            .with_source_file(1),
        );
        rows.modules.push(
            ModuleInput::package(ModuleId(11), "one", "pkg/one.js", "pkg", None)
                .with_source_file(2),
        );
        rows.modules.push(
            ModuleInput::package(ModuleId(12), "two", "pkg/two.js", "pkg", None)
                .with_source_file(3),
        );
        rows.modules.push(
            ModuleInput::package(ModuleId(13), "three", "pkg/three.js", "pkg", None)
                .with_source_file(4),
        );
        for (from, to) in [
            (ModuleId(10), ModuleId(11)),
            (ModuleId(11), ModuleId(12)),
            (ModuleId(12), ModuleId(13)),
        ] {
            rows.dependencies.push(ModuleDependencyInput {
                from_module_id: from,
                target: ModuleDependencyTarget::Module(to),
            });
        }
        let package_sources = [
            PackageSource::external(
                "pkg",
                "1.2.3",
                "pkg/one",
                "one.js",
                "export function one(){return 'one-anchor';}",
            ),
            PackageSource::external(
                "pkg",
                "1.2.3",
                "pkg/two",
                "two.js",
                "export function two(){return 'two-anchor';}",
            ),
            PackageSource::external(
                "pkg",
                "1.2.3",
                "pkg/three",
                "three.js",
                "export function three(){return 'three-anchor';}",
            ),
        ];

        let report = match_packages_with_pipeline(&rows, &package_sources, None);

        assert!(report.package_report.audit.is_clean());
        assert_eq!(report.package_report.matches.len(), 4);
        let cluster_match = report
            .package_report
            .matches
            .iter()
            .find(|package_match| package_match.module_id == ModuleId(10))
            .expect("cluster member should be promoted");
        assert!(
            cluster_match
                .source_path
                .contains("dependency-cluster:pkg@1.2.3")
        );
        assert!(cluster_match.source_path.contains("owned_seeds=3/3"));
        assert!(!cluster_match.external_importable);
    }

    #[test]
    fn pipeline_promotes_same_file_package_graph_ownership() {
        let one = "export function one(){return 'one-anchor';}";
        let gap = "const localValue = Math.random();";
        let two = "export function two(){return 'two-anchor';}";
        let tail = "const trailingValue = Date.now();";
        let bundled = [one, gap, two, tail].join("\n");
        let one_start = 0usize;
        let one_end = one.len();
        let gap_start = one_end + 1;
        let gap_end = gap_start + gap.len();
        let two_start = gap_end + 1;
        let two_end = two_start + two.len();
        let tail_start = two_end + 1;
        let tail_end = tail_start + tail.len();
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(bundled)));
        rows.modules.push(
            ModuleInput::package(
                ModuleId(10),
                "one",
                "pkg/one.js",
                "pkg",
                Some("1.2.3".to_string()),
            )
            .with_source_file(1)
            .with_source_span(SourceSpan::new(one_start as u32, one_end as u32)),
        );
        rows.modules.push(
            ModuleInput::package(ModuleId(11), "gap", "pkg/absent-item.js", "pkg", None)
                .with_source_file(1)
                .with_source_span(SourceSpan::new(gap_start as u32, gap_end as u32)),
        );
        rows.modules.push(
            ModuleInput::package(
                ModuleId(12),
                "two",
                "pkg/two.js",
                "pkg",
                Some("1.2.3".to_string()),
            )
            .with_source_file(1)
            .with_source_span(SourceSpan::new(two_start as u32, two_end as u32)),
        );
        rows.modules.push(
            ModuleInput::package(ModuleId(13), "tail", "pkg/unused-tail.js", "pkg", None)
                .with_source_file(1)
                .with_source_span(SourceSpan::new(tail_start as u32, tail_end as u32)),
        );
        let package_sources = [
            PackageSource::external("pkg", "1.2.3", "pkg/one", "one.js", one),
            PackageSource::external("pkg", "1.2.3", "pkg/two", "two.js", two),
        ];

        let report = match_packages_with_pipeline(&rows, &package_sources, None);

        assert!(report.package_report.audit.is_clean());
        assert_eq!(
            report.package_report.matches.len(),
            4,
            "same-file package run should rescue parseable modules without dependency edges"
        );
        let gap_match = report
            .package_report
            .matches
            .iter()
            .find(|package_match| package_match.module_id == ModuleId(11))
            .expect("same-file package graph should promote gap module");
        assert_eq!(
            gap_match.strategy,
            ModuleMatchStrategy::DependencyClosureOwnership
        );
        assert!(
            gap_match
                .source_path
                .contains("package-file-graph:pkg@1.2.3"),
            "{}",
            gap_match.source_path
        );
        assert!(gap_match.source_path.contains("owned_seeds=2/2"));
        assert!(gap_match.source_path.contains("run_size=4"));
        assert!(!gap_match.external_importable);
    }

    #[test]
    fn unversioned_exact_match_does_not_infer_package_version() {
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

        let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

        assert!(report.attributions.is_empty());
        assert!(report.matches.is_empty());
        assert!(report.version_matches.is_empty());
        assert!(report.audit.is_clean());
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
            ModuleInput::package(
                ModuleId(10),
                "one",
                "pkg/one.ts",
                "pkg",
                Some("1.0.0".to_string()),
            )
            .with_source_file(1)
            .with_source_span(SourceSpan::new(0, 21)),
        );
        rows.modules.push(
            ModuleInput::package(
                ModuleId(11),
                "two",
                "pkg/two.ts",
                "pkg",
                Some("1.0.0".to_string()),
            )
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

        let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

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

        let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

        assert!(report.attributions.is_empty());
        assert!(report.matches.is_empty());
        assert!(report.audit.is_clean());
    }

    #[test]
    fn versioned_matcher_uses_exact_package_version_hint_over_sorted_hashes() {
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
        let rows = rows_with_package_source_at_version("export const target=42", "2.0.0");
        let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

        assert!(report.audit.is_clean());
        assert_eq!(report.attributions.len(), 1);
        assert_eq!(
            report.attributions[0].package_version.as_deref(),
            Some("2.0.0")
        );
        assert_eq!(
            report.matches[0].strategy,
            ModuleMatchStrategy::NormalizedSourceHash
        );
    }

    #[test]
    fn versioned_matcher_uses_explicit_package_version_for_module_group() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some("export const one = 1;\nexport const two = 2;".to_string()),
        ));
        rows.modules.push(
            ModuleInput::package(
                ModuleId(10),
                "one",
                "pkg/one.ts",
                "pkg",
                Some("2.0.0".to_string()),
            )
            .with_source_file(1)
            .with_source_span(SourceSpan::new(0, 21)),
        );
        rows.modules.push(
            ModuleInput::package(
                ModuleId(11),
                "two",
                "pkg/two.ts",
                "pkg",
                Some("2.0.0".to_string()),
            )
            .with_source_file(1)
            .with_source_span(SourceSpan::new(22, 43)),
        );
        let package_sources = [
            PackageSource::external("pkg", "1.0.0", "pkg/one", "one.js", "export const one = 1;"),
            PackageSource::external("pkg", "2.0.0", "pkg/one", "one.js", "export const one = 1;"),
            PackageSource::external("pkg", "2.0.0", "pkg/two", "two.js", "export const two = 2;"),
        ];

        let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

        assert!(report.audit.is_clean());
        assert_eq!(report.attributions.len(), 2);
        assert!(
            report
                .attributions
                .iter()
                .all(|attribution| attribution.package_version.as_deref() == Some("2.0.0"))
        );
        let selected = report
            .version_matches
            .iter()
            .find_map(|decision| match decision {
                BestVersionMatch::Selected { score, .. } => Some(score),
                _ => None,
            })
            .expect("exact version should be selected");
        assert_eq!(selected.package_version, "2.0.0");
        assert_eq!(selected.total_modules, 2);
        assert_eq!(selected.matched_modules, 2);
        assert!(selected.binary_search_probes > 0);
    }

    #[test]
    fn versioned_matcher_uses_exact_module_version_hints_per_version() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle-one.js",
            Some("export const one=1;".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            2,
            "bundle-two.js",
            Some("export const two=2;".to_string()),
        ));
        rows.modules.push(
            ModuleInput::package(
                ModuleId(10),
                "one",
                "node_modules/pkg/one.js",
                "pkg",
                Some("1.0.0".to_string()),
            )
            .with_source_file(1),
        );
        rows.modules.push(
            ModuleInput::package(
                ModuleId(11),
                "two",
                "node_modules/pkg/two.js",
                "pkg",
                Some("2.0.0".to_string()),
            )
            .with_source_file(2),
        );
        let package_sources = [
            PackageSource::external("pkg", "1.0.0", "pkg/one", "one.js", "export const one = 1;"),
            PackageSource::external("pkg", "2.0.0", "pkg/two", "two.js", "export const two = 2;"),
        ];

        let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

        assert!(report.audit.is_clean());
        assert_eq!(report.attributions.len(), 2);
        assert_eq!(
            report
                .attributions
                .iter()
                .map(|attribution| (
                    attribution.module_id,
                    attribution.package_version.as_deref()
                ))
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([(ModuleId(10), Some("1.0.0")), (ModuleId(11), Some("2.0.0")),])
        );
        let selected_versions = report
            .version_matches
            .iter()
            .filter_map(|decision| match decision {
                BestVersionMatch::Selected { score, .. } => Some(score.package_version.as_str()),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(selected_versions, BTreeSet::from(["1.0.0", "2.0.0"]));
    }

    #[test]
    fn unversioned_equal_sources_do_not_infer_package_version() {
        let rows = rows_with_package_source("export const value=1");
        let package_sources = [
            PackageSource::external(
                "pkg",
                "1.0.0",
                "pkg/value",
                "value.js",
                "export const value = 1;",
            ),
            PackageSource::external(
                "pkg",
                "2.0.0",
                "pkg/value",
                "value.js",
                "export const value = 1;",
            ),
        ];

        let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

        assert!(report.attributions.is_empty());
        assert!(report.matches.is_empty());
        assert!(report.version_matches.is_empty());
        assert!(report.audit.is_clean());
    }

    #[test]
    fn versioned_matcher_can_match_by_function_signatures_and_string_anchors() {
        let rows = rows_with_package_source_at_version(
            "const bundleMarker = 1;\nexport function first(){return 'stable-anchor'}\nexport function second(){return 'other-anchor'}",
            "1.0.0",
        );
        let package_sources = [PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/functions",
            "functions.js",
            "function first(){return 'stable-anchor'}\nfunction second(){return 'other-anchor'}",
        )];

        let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

        assert!(report.audit.is_clean());
        assert_eq!(report.attributions.len(), 1);
        assert_eq!(
            report.matches[0].strategy,
            ModuleMatchStrategy::FunctionSignatureAndStringAnchors
        );
        assert!(report.matches[0].function_signature_matches >= 2);
        assert!(report.matches[0].string_anchor_matches >= 1);
    }

    #[test]
    fn source_package_imports_are_extracted_from_whole_source_file() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some(
                "import { x } from 'pkg/sub';\nconst y = require('undici');\nasync function f(){ return import('ws'); }\nimport fs from 'node:fs';"
                    .to_string(),
            ),
        ));

        let names = package_import_names_from_sources(&rows);

        assert!(names.contains("pkg"));
        assert!(names.contains("undici"));
        assert!(names.contains("ws"));
        assert!(!names.contains("node:fs"));
        assert!(!names.contains("fs"));
    }

    #[test]
    fn source_backed_import_surface_uses_unique_project_package_version() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some("const client = require('undici');".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "undici_wrapper",
            "pkg/undici.ts",
            "undici",
            Some("2.2.1".to_string()),
        ));
        rows.package_attributions
            .push(PackageAttributionInput::accepted_external(
                ModuleId(10),
                "undici",
                "2.2.1",
                "undici",
            ));

        let report = VersionedPackageMatcher::default().match_rows(&rows, &[]);

        assert!(report.audit.is_clean());
        assert_eq!(report.surfaces.len(), 1);
        assert_eq!(report.surfaces[0].package_name, "undici");
        assert_eq!(report.surfaces[0].package_version.as_deref(), Some("2.2.1"));
        assert_eq!(report.surfaces[0].export_specifier, "undici");
        assert!(
            report.surfaces[0]
                .evidence
                .as_deref()
                .is_some_and(|evidence| evidence.contains("source_package_import_surface"))
        );
    }

    #[test]
    fn source_backed_import_surface_uses_wildcard_for_ambiguous_cached_versions() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some("const ws = require('ws');".to_string()),
        ));
        let package_sources = [
            PackageSource::external("ws", "8.0.0", "ws", "wrapper.mjs", "export default {};"),
            PackageSource::external(
                "ws",
                "8.18.2",
                "ws",
                "lib/websocket-server.js",
                "export class WebSocketServer {}",
            ),
        ];

        let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

        assert_eq!(report.surfaces.len(), 1);
        assert_eq!(report.surfaces[0].package_name, "ws");
        assert_eq!(report.surfaces[0].package_version.as_deref(), Some("*"));
        assert!(
            report.surfaces[0]
                .evidence
                .as_deref()
                .is_some_and(|evidence| evidence.contains("source_import_without_unique_version"))
        );
    }
}
