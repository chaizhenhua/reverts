pub mod cascade;
pub mod tier;
pub mod variant;
pub mod version;
pub use cascade::*;
pub use tier::*;
pub use variant::*;
pub use version::*;

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
use oxc_parser::{ParseOptions, Parser};
use reverts_input::{
    InputRows, ModuleInput, PackageAttributionInput, PackageAttributionStatus, PackageEmissionMode,
    PackageSurfaceInput,
};
use reverts_ir::hash::{
    FNV_OFFSET_BASIS, fnv1a_hex as stable_hash, update_fnv1a as update_stable_hash,
};
use reverts_ir::{ModuleId, ModuleKind, is_valid_package_name, split_bare_specifier};
use reverts_js::{
    JsError, ParseError, ParseGoal, normalize_source_for_pipeline, parse_error_message,
    source_type_candidates,
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
    /// stable hash of the AST-normalized source body.
    pub normalized_source_hash: String,
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
}

impl ModuleMatchStrategy {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NormalizedSourceHash => "normalized_source_hash",
            Self::FunctionSignatureAndStringAnchors => "function_signature_and_string_anchors",
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

    /// Matches unresolved package modules to the best concrete package version.
    #[must_use]
    pub fn match_rows(
        &self,
        rows: &InputRows,
        package_sources: &[PackageSource],
    ) -> VersionedPackageMatchReport {
        let mut audit = AuditReport::default();
        let index = PackageVersionIndex::build(package_sources, &mut audit);
        let mut decisions = Vec::new();
        let mut matches = Vec::new();
        let mut attributions = Vec::new();

        for package_name in package_names_for_matching(rows) {
            let module_fingerprints =
                fingerprint_modules_for_package(rows, package_name.as_str(), &mut audit);
            if module_fingerprints.is_empty() {
                continue;
            }

            let decision = index.best_version_for_package(
                package_name.as_str(),
                &module_fingerprints,
                &self.config,
            );
            if let BestVersionMatch::Selected {
                score: _score,
                module_matches,
            } = &decision
            {
                for module_match in module_matches {
                    attributions.push(accepted_attribution_from_match(module_match));
                    matches.push(PackageMatch::from_module_match(module_match));
                }
            } else if let BestVersionMatch::Ambiguous {
                package_name,
                scores: _scores,
            } = &decision
            {
                audit.push(
                    AuditFinding::error(
                        FindingCode::AmbiguousPackageMatch,
                        "package version search found more than one best version",
                    )
                    .with_binding(package_name.clone()),
                );
            }
            decisions.push(decision);
        }
        let surfaces = resolve_source_package_surfaces(rows, &attributions, package_sources);

        VersionedPackageMatchReport {
            attributions,
            surfaces,
            matches,
            version_matches: decisions,
            audit,
        }
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
    /// Per-package best-version decisions.
    pub version_matches: Vec<BestVersionMatch>,
    /// Ambiguity, missing source, and parse findings.
    pub audit: AuditReport,
}

impl From<VersionedPackageMatchReport> for PackageMatchReport {
    fn from(report: VersionedPackageMatchReport) -> Self {
        Self {
            attributions: report.attributions,
            surfaces: report.surfaces,
            matches: report.matches,
            audit: report.audit,
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
        VersionedPackageMatcher::default()
            .match_rows(rows, package_sources)
            .into()
    }
}

#[derive(Debug, Clone, PartialEq)]
/// Result of a package matching pass.
pub struct PackageMatchReport {
    /// Accepted attributions that can be persisted by the caller.
    pub attributions: Vec<PackageAttributionInput>,
    /// Accepted project-level package surfaces discovered from source-backed bare imports.
    pub surfaces: Vec<PackageSurfaceInput>,
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
    /// strategy that proved this module/source match.
    pub strategy: ModuleMatchStrategy,
    /// overlapping function signatures.
    pub function_signature_matches: usize,
    /// overlapping string anchors.
    pub string_anchor_matches: usize,
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

fn package_names_for_matching(rows: &InputRows) -> BTreeSet<String> {
    rows.modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
        .filter(|module| !has_accepted_attribution(rows, module.id))
        .filter_map(|module| module.package_name.clone())
        .collect()
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
) -> Vec<PackageSurfaceInput> {
    let mut sites_by_specifier = BTreeMap::<(String, String), BTreeSet<String>>::new();
    for site in package_import_sites_from_sources(rows) {
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

    fn best_version_for_package(
        &self,
        package_name: &str,
        module_fingerprints: &[ModuleMatchFingerprint],
        config: &VersionedPackageMatcherConfig,
    ) -> BestVersionMatch {
        let Some(versions) = self.packages.get(package_name) else {
            return BestVersionMatch::NoMatch {
                package_name: package_name.to_string(),
                scores: Vec::new(),
            };
        };
        if versions.is_empty() {
            return BestVersionMatch::NoMatch {
                package_name: package_name.to_string(),
                scores: Vec::new(),
            };
        }

        let mut scored = binary_search_best_version(versions, module_fingerprints, config);
        certify_version_scores(versions, module_fingerprints, config, &mut scored);
        let Some(best_score) = scored
            .iter()
            .map(|scored| &scored.score)
            .max_by(|left, right| left.score.cmp(&right.score))
        else {
            return BestVersionMatch::NoMatch {
                package_name: package_name.to_string(),
                scores: Vec::new(),
            };
        };

        if !best_score.has_evidence() {
            return BestVersionMatch::NoMatch {
                package_name: package_name.to_string(),
                scores: scored.into_iter().map(|scored| scored.score).collect(),
            };
        }

        let tied_best = scored
            .iter()
            .filter(|scored| scored.score.score == best_score.score)
            .collect::<Vec<_>>();
        if tied_best.len() > 1 {
            return BestVersionMatch::Ambiguous {
                package_name: package_name.to_string(),
                scores: tied_best
                    .into_iter()
                    .map(|scored| scored.score.clone())
                    .collect(),
            };
        }

        if best_score.source_hash_matches == 0
            && (best_score.function_signature_matches < config.min_function_signature_matches
                || best_score.string_anchor_matches < config.min_string_anchor_matches)
        {
            return BestVersionMatch::InsufficientEvidence {
                score: best_score.clone(),
            };
        }

        let best_package_version = best_score.package_version.clone();
        let selected = scored
            .into_iter()
            .find(|scored| scored.score.package_version == best_package_version)
            .expect("selected score was computed");
        BestVersionMatch::Selected {
            score: selected.score,
            module_matches: selected.module_matches,
        }
    }
}

#[derive(Debug)]
struct ScoredPackageVersion {
    score: VersionMatchScore,
    module_matches: Vec<ModulePackageMatch>,
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

fn module_match_fingerprint(
    module: &ModuleInput,
    path: &str,
    source: &str,
) -> Result<ModuleMatchFingerprint, String> {
    let source_fingerprint = fingerprint_source(path, source)?;
    Ok(ModuleMatchFingerprint {
        module_id: module.id,
        package_name: module.package_name.clone(),
        normalized_source_hash: source_fingerprint.normalized_source_hash,
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
        function_signature_hashes: fingerprint.function_signature_hashes,
        string_anchors: fingerprint.string_anchors,
    })
}

#[derive(Debug)]
struct SourceFingerprint {
    normalized_source_hash: String,
    function_signature_hashes: BTreeSet<String>,
    string_anchors: BTreeSet<String>,
}

fn fingerprint_source(path: &str, source: &str) -> Result<SourceFingerprint, String> {
    let normalized = normalize_source(path, source)?;
    let ast = ast_fingerprint(path, normalized.as_str())?;
    Ok(SourceFingerprint {
        normalized_source_hash: stable_hash(normalized.as_bytes()),
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

fn parse_options_for(source_type: oxc_span::SourceType) -> ParseOptions {
    ParseOptions {
        allow_return_outside_function: source_type.is_script(),
        ..Default::default()
    }
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

fn binary_search_best_version<'a>(
    versions: &'a [PackageVersionCandidate<'a>],
    module_fingerprints: &[ModuleMatchFingerprint],
    config: &VersionedPackageMatcherConfig,
) -> Vec<ScoredPackageVersion> {
    let mut scores = BTreeMap::<usize, ScoredPackageVersion>::new();
    if versions.is_empty() {
        return Vec::new();
    }

    let mut low = 0usize;
    let mut high = versions.len() - 1;
    while low <= high {
        let mid = low + (high - low) / 2;
        score_version_at(mid, versions, module_fingerprints, config, &mut scores);
        if mid > 0 {
            score_version_at(mid - 1, versions, module_fingerprints, config, &mut scores);
        }
        if mid < versions.len() - 1 {
            score_version_at(mid + 1, versions, module_fingerprints, config, &mut scores);
        }

        let mid_score = scores.get(&mid).expect("midpoint was scored").score.score;
        let left_score = mid
            .checked_sub(1)
            .and_then(|index| scores.get(&index))
            .map_or(0, |score| score.score.score);
        let right_score = scores.get(&(mid + 1)).map_or(0, |score| score.score.score);

        if right_score > mid_score && mid < high {
            low = mid + 1;
        } else if left_score > mid_score && mid > low {
            high = mid - 1;
        } else {
            break;
        }
    }

    let probe_count = scores.len();
    scores
        .into_values()
        .map(|mut scored| {
            scored.score.binary_search_probes = probe_count;
            scored
        })
        .collect()
}

fn certify_version_scores<'a>(
    versions: &'a [PackageVersionCandidate<'a>],
    module_fingerprints: &[ModuleMatchFingerprint],
    config: &VersionedPackageMatcherConfig,
    scored: &mut Vec<ScoredPackageVersion>,
) {
    let mut existing = scored
        .iter()
        .map(|scored| scored.score.package_version.clone())
        .collect::<BTreeSet<_>>();
    let probe_count = scored
        .first()
        .map_or(0, |scored| scored.score.binary_search_probes);

    for version in versions {
        if existing.insert(version.package_version.clone()) {
            let mut score = score_version(version, module_fingerprints, config);
            score.score.binary_search_probes = probe_count;
            scored.push(score);
        }
    }
}

fn score_version_at<'a>(
    index: usize,
    versions: &'a [PackageVersionCandidate<'a>],
    module_fingerprints: &[ModuleMatchFingerprint],
    config: &VersionedPackageMatcherConfig,
    scores: &mut BTreeMap<usize, ScoredPackageVersion>,
) {
    scores
        .entry(index)
        .or_insert_with(|| score_version(&versions[index], module_fingerprints, config));
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
    let exact_candidates = candidates_for_source_hash(
        version.sources.as_slice(),
        module.normalized_source_hash.as_str(),
    );
    if let Some(candidates) = exact_candidates {
        if let Some(source) = unique_source_candidate(candidates) {
            return Some(module_package_match(
                module,
                source,
                ModuleMatchStrategy::NormalizedSourceHash,
                source
                    .function_signature_hashes
                    .intersection(&module.function_signature_hashes)
                    .count(),
                source
                    .string_anchors
                    .intersection(&module.string_anchors)
                    .count(),
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
            .then_with(|| left.0.source.source_path.cmp(&right.0.source.source_path))
    });

    let best = ranked.first()?;
    if ranked
        .get(1)
        .is_some_and(|next| next.1 == best.1 && next.2 == best.2)
    {
        return None;
    }

    Some(module_package_match(
        module,
        best.0,
        ModuleMatchStrategy::FunctionSignatureAndStringAnchors,
        best.1,
        best.2,
    ))
}

fn candidates_for_source_hash<'a>(
    sources: &'a [PackageSourceFingerprint<'a>],
    hash: &str,
) -> Option<&'a [PackageSourceFingerprint<'a>]> {
    let index = sources
        .binary_search_by(|source| source.normalized_source_hash.as_str().cmp(hash))
        .ok()?;

    let mut start = index;
    while start > 0 && sources[start - 1].normalized_source_hash == hash {
        start -= 1;
    }

    let mut end = index + 1;
    while end < sources.len() && sources[end].normalized_source_hash == hash {
        end += 1;
    }

    Some(&sources[start..end])
}

fn unique_source_candidate<'a>(
    candidates: &'a [PackageSourceFingerprint<'a>],
) -> Option<&'a PackageSourceFingerprint<'a>> {
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
        candidates.first()
    } else {
        None
    }
}

fn module_package_match(
    module: &ModuleMatchFingerprint,
    source: &PackageSourceFingerprint<'_>,
    strategy: ModuleMatchStrategy,
    function_signature_matches: usize,
    string_anchor_matches: usize,
) -> ModulePackageMatch {
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
    }
}

fn accepted_attribution_from_match(module_match: &ModulePackageMatch) -> PackageAttributionInput {
    let mut attribution = PackageAttributionInput::accepted_external(
        module_match.module_id,
        module_match.package_name.as_str(),
        module_match.package_version.as_str(),
        module_match.export_specifier.as_str(),
    );
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

#[cfg(test)]
mod tests {
    use reverts_input::{
        InputRows, ModuleInput, PackageAttributionInput, ProjectInput, SourceFileInput, SourceSpan,
    };
    use reverts_ir::ModuleId;
    use reverts_observe::FindingCode;

    use super::{
        BestVersionMatch, ExactPackageMatcher, ModuleMatchStrategy, PackageSource,
        VersionedPackageMatcher, package_import_names_from_sources,
    };

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
    fn versioned_matcher_uses_binary_lookup_over_sorted_hashes() {
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
        let rows = rows_with_package_source("export const target=42");
        let report = ExactPackageMatcher.match_rows(&rows, &package_sources);

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
    fn versioned_matcher_selects_best_package_version_by_module_score() {
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
            .expect("best version should be selected");
        assert_eq!(selected.package_version, "2.0.0");
        assert_eq!(selected.total_modules, 2);
        assert_eq!(selected.matched_modules, 2);
        assert!(selected.binary_search_probes > 0);
    }

    #[test]
    fn versioned_matcher_rejects_equal_best_versions() {
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
        assert!(report.audit.has(FindingCode::AmbiguousPackageMatch));
        assert!(
            report
                .version_matches
                .iter()
                .any(|decision| matches!(decision, BestVersionMatch::Ambiguous { .. }))
        );
    }

    #[test]
    fn versioned_matcher_can_match_by_function_signatures_and_string_anchors() {
        let rows = rows_with_package_source(
            "export function first(){return 'stable-anchor'}\nexport function second(){return 'other-anchor'}",
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

        let report = ExactPackageMatcher.match_rows(&rows, &[]);

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

        let report = ExactPackageMatcher.match_rows(&rows, &package_sources);

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
