pub mod acceptance;
mod binding_signatures;
pub mod cascade;
pub mod cascade_match;
mod externalization_policy;
pub mod hungarian;
mod index;
mod model;
mod ownership;
pub mod package_helpers;
mod source;
pub mod structural_bag;
pub mod tier;
mod version_scoring;

pub use acceptance::{AcceptanceDecision, classify};
use binding_signatures::binding_string_signatures_from_source;
pub use cascade::{GlobalAssignment, assign_globally, cascade_candidates, match_function};
pub use cascade_match::{CascadeMatchReport, CascadeOwnershipMatch, match_with_cascade};
use externalization_policy::{
    SemanticExternalTargetPolicy, canonical_subpath_policy_allows,
    cross_package_exact_source_policy_allows, dependency_edge_path_policy_allows,
    dependency_graph_source_fingerprint_policy_allows, dependency_graph_source_proof_label,
    dependency_graph_source_proof_rank, dependency_graph_source_proof_requires_unique_source_path,
    export_member_source_proof_alias_source_is_matched, export_member_source_proof_label,
    export_member_source_proof_rank, public_export_member_policy_allows,
    same_package_cross_version_source_policy_allows, semantic_external_source_proof_label,
    semantic_external_source_proof_rank, semantic_external_target_policies,
    semantic_source_only_export_member_policy_allows, source_only_match_can_be_promoted_to_import,
};
pub use hungarian::assign_max_weight;
pub(crate) use index::ExternalImportSourceIndex;
pub use index::package_module_source_quality;
use index::{
    PackageVersionIndex, fingerprint_modules_for_package, fingerprint_source,
    is_strong_path_hint_token,
};
pub(crate) use index::{module_match_fingerprint, package_source_fingerprint};
pub(crate) use model::PACKAGE_SOURCE_FINGERPRINT_MAX_BYTES;
pub use model::{
    BestVersionMatch, ModuleMatchFingerprint, ModuleMatchStrategy, ModulePackageMatch,
    PackageImportSite, PackageMatch, PackageMatchingPipelineReport, PackageModuleSourceQuality,
    PackagePublicExportProof, PackageSource, PackageSourceFingerprint, PackageVersionCandidate,
    SourcePackageImportParseError, VersionMatchScore, VersionedPackageMatchReport,
    VersionedPackageMatcherConfig,
};
pub(crate) use model::{
    ConcretePackageSourcePath, CorrectedPackageExternalImportTarget, ExternalImportTarget,
    package_module_source_quality_label,
};
pub(crate) use ownership::dependency_closure::{
    DependencyNeighborhoodEvidence, dependency_neighborhood_ownership_evidence,
    dependency_neighborhood_source_path, has_direct_neighborhood_package_contradiction,
    package_dependency_components,
};
pub use package_helpers::{
    SemanticPathHintMode, accepted_external_modules, canonical_public_path_segments,
    clean_package_semantic_path_hint, direct_module_dependencies, direct_module_dependents,
    has_accepted_external_attribution, is_build_path_segment, is_exact_package_version_hint,
    is_json_source_path, module_package_semantic_path_hints, normalize_hint_text,
    ownership_by_module, package_semantic_path_prefixes, package_source_entry_path,
    package_source_export_path, package_source_external_import_rank, package_source_relative_path,
    package_source_semantic_hint_score, package_source_semantic_surface_hint_score,
    path_hint_tokens, strip_package_prefix_from_semantic_path, strip_source_extension,
};
use source::exported_members::{
    export_member_set_is_strong, exported_members_from_source, is_identifier_name,
    is_usable_export_member,
};
use source::import_targets::{
    commonjs_reexport_targets, export_all_reexport_targets, reexport_targets,
    relative_module_specifier_targets,
};
pub(crate) use source::source_imports::resolve_source_package_surfaces;
pub use source::source_imports::{
    package_import_names_from_sources, package_import_sites_from_sources,
};
pub(crate) use source::source_text::normalize_source;
pub use structural_bag::{
    StructuralBagMatchReport, match_structural_bags, match_structural_bags_with_excluded_modules,
};
pub use tier::{
    FunctionMatch, STRUCTURAL_FREQUENCY_LIMIT, try_exact, try_exact_alternate,
    try_feature_similarity, try_structural_anchored, try_structural_only,
};
use version_scoring::best_source_match;
pub(crate) use version_scoring::{
    accepted_attribution_from_match, disambiguate_exact_source_candidate, module_package_match,
};

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use reverts_graph::FunctionExtractor;
use reverts_input::{
    InputRows, ModuleInput, PackageAttributionInput, PackageAttributionStatus, PackageEmissionMode,
};
use reverts_ir::hash::fnv1a_hex as stable_hash;
use reverts_ir::{ModuleId, ModuleKind, is_valid_package_name};
use reverts_observe::{AuditFinding, AuditReport, FindingCode};
use reverts_package::external_import_concrete_source_path;
use semver::Version;

#[must_use]
pub fn package_source_normalized_hash(path: &str, source: &str) -> Option<String> {
    normalize_source(path, source)
        .ok()
        .map(|normalized| stable_hash(normalized.as_bytes()))
}

#[must_use]
pub fn package_source_exported_members(path: &str, source: &str) -> BTreeSet<String> {
    exported_members_from_source(path, source)
}

#[must_use]
pub fn package_source_public_export_proofs(
    package_sources: &[PackageSource],
) -> Vec<PackagePublicExportProof> {
    let external_source_index = ExternalImportSourceIndex::build(package_sources);
    let mut candidates_by_source_path =
        BTreeMap::<String, Vec<(&PackageSource, BTreeSet<String>)>>::new();

    for external in package_sources
        .iter()
        .filter(|source| source.external_importable)
    {
        for source in reexported_source_only_sources(external, &external_source_index) {
            let public_members = external_source_index
                .export_members(source)
                .into_iter()
                .filter(|member| is_usable_export_member(member))
                .collect::<BTreeSet<_>>();
            if !export_member_set_is_strong(public_members.iter()) {
                continue;
            }
            candidates_by_source_path
                .entry(source.source_path.clone())
                .or_default()
                .push((external, public_members));
        }
    }

    let mut proofs = Vec::new();
    for (source_path, mut candidates) in candidates_by_source_path {
        candidates.sort_by(|left, right| {
            package_source_external_import_rank(left.0)
                .cmp(&package_source_external_import_rank(right.0))
                .then_with(|| left.0.export_specifier.cmp(&right.0.export_specifier))
                .then_with(|| left.0.source_path.cmp(&right.0.source_path))
        });
        let Some((best_external, _)) = candidates.first() else {
            continue;
        };
        let best_rank = package_source_external_import_rank(best_external);
        let best = candidates
            .into_iter()
            .filter(|(external, _)| package_source_external_import_rank(external) == best_rank)
            .collect::<Vec<_>>();
        let export_specifiers = best
            .iter()
            .map(|(external, _)| external.export_specifier.as_str())
            .collect::<BTreeSet<_>>();
        if export_specifiers.len() != 1 {
            continue;
        }
        let export_specifier = export_specifiers
            .into_iter()
            .next()
            .expect("one export specifier")
            .to_string();
        let Some((source, public_members)) =
            best.into_iter().next().and_then(|(external, members)| {
                external_source_index
                    .all_sources_for_package(external.package_name.as_str())
                    .into_iter()
                    .find(|candidate| candidate.source_path == source_path)
                    .map(|source| (source, members))
            })
        else {
            continue;
        };
        proofs.push(PackagePublicExportProof {
            package_name: source.package_name.clone(),
            package_version: source.package_version.clone(),
            source_path,
            export_specifier,
            public_members,
        });
    }

    proofs.sort_by(|left, right| {
        left.package_name
            .cmp(&right.package_name)
            .then_with(|| left.package_version.cmp(&right.package_version))
            .then_with(|| left.source_path.cmp(&right.source_path))
            .then_with(|| left.export_specifier.cmp(&right.export_specifier))
    });
    proofs
}

fn reexported_source_only_sources<'a>(
    external: &'a PackageSource,
    external_source_index: &'a ExternalImportSourceIndex<'a>,
) -> Vec<&'a PackageSource> {
    let mut results = BTreeMap::<String, &'a PackageSource>::new();
    let mut visited = BTreeSet::<String>::new();
    let mut stack = vec![external];
    while let Some(source) = stack.pop() {
        if !visited.insert(source.source_path.clone()) {
            continue;
        }
        for entry in package_source_reexport_entries(source) {
            for target in sources_matching_entry(
                source.package_name.as_str(),
                source.package_version.as_str(),
                entry.as_str(),
                external_source_index,
            ) {
                if target.external_importable {
                    continue;
                }
                results.entry(target.source_path.clone()).or_insert(target);
                stack.push(target);
            }
        }
    }
    results.into_values().collect()
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
        let surfaces = resolve_source_package_surfaces(
            rows,
            &attributions,
            package_sources,
            package_filter,
            &mut audit,
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

/// Runs the complete package matching pipeline through one matcher-owned
/// entry point. Matched package modules are always externalized; the matcher no
/// longer exposes a proof-only source path.
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
    let timing_enabled = std::env::var_os("REVERTS_MATCH_TIMING").is_some();
    let timing_started = Instant::now();
    let mut timing_last = timing_started;
    macro_rules! mark_timing {
        ($stage:literal) => {
            if timing_enabled {
                let now = Instant::now();
                eprintln!(
                    "package-pipeline timing: {} stage={:.3}s total={:.3}s",
                    $stage,
                    now.duration_since(timing_last).as_secs_f64(),
                    now.duration_since(timing_started).as_secs_f64()
                );
                timing_last = now;
            }
        };
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
    mark_timing!("versioned_matcher");

    let skip_cascade = package_sources.len() > CASCADE_PIPELINE_SOURCE_LIMIT;
    let package_matched_modules = if package_sources.len() > CASCADE_MATCHED_MODULE_SOURCE_LIMIT {
        package_report
            .matches
            .iter()
            .map(|package_match| package_match.module_id)
            .collect::<BTreeSet<_>>()
    } else {
        BTreeSet::new()
    };
    let fingerprints_by_module = if skip_cascade {
        BTreeMap::new()
    } else {
        fingerprints_from_rows(
            rows,
            package_filter,
            &package_matched_modules,
            package_sources.len() > CASCADE_MATCHED_MODULE_SOURCE_LIMIT,
        )
    };
    mark_timing!("module_function_fingerprints");
    let cascade_report = if skip_cascade {
        CascadeMatchReport {
            attributions: Vec::new(),
            ownership_matches: Vec::new(),
            audit: AuditReport::default(),
        }
    } else {
        match_with_cascade_scoped_by_module_hints(rows, &fingerprints_by_module, package_sources)
    };
    mark_timing!("cascade_match");
    ownership::cascade::promote_cascade_function_coverage_to_module_attributions(
        rows,
        &fingerprints_by_module,
        &cascade_report,
        &mut package_report,
    );
    mark_timing!("cascade_promote");
    let function_attributions = cascade_report.attributions;
    let function_ownership_matches = cascade_report.ownership_matches.len();
    package_report.audit.extend(cascade_report.audit);

    let structural_bag_report = if skip_cascade {
        StructuralBagMatchReport {
            matches: Vec::new(),
            audit: AuditReport::default(),
        }
    } else {
        let structural_bag_excluded_modules = package_report
            .matches
            .iter()
            .map(|package_match| package_match.module_id)
            .collect::<BTreeSet<_>>();
        match_structural_bags_with_excluded_modules(
            rows,
            package_sources,
            package_filter,
            &structural_bag_excluded_modules,
        )
    };
    mark_timing!("structural_bag");
    structural_bag::promote_structural_bag_ownership_matches(
        rows,
        structural_bag_report.matches.as_slice(),
        &mut package_report,
    );
    mark_timing!("structural_promote");
    package_report.audit.extend(structural_bag_report.audit);
    ownership::weak_source_equivalent::promote_weak_source_equivalent_matches(
        rows,
        package_sources,
        &mut package_report,
    );
    mark_timing!("weak_source_equivalent");
    ownership::exact_hint::promote_exact_hint_ownership_matches(
        rows,
        package_sources,
        &mut package_report,
    );
    mark_timing!("exact_hint_promote");
    ownership::dependency_neighborhood::promote_dependency_closure_ownership_matches(
        rows,
        &mut package_report,
    );
    mark_timing!("dependency_closure");
    ownership::dependency_neighborhood::promote_dependency_cluster_ownership_matches(
        rows,
        &mut package_report,
    );
    mark_timing!("dependency_cluster");
    ownership::package_file_graph::promote_package_file_graph_ownership_matches(
        rows,
        &mut package_report,
    );
    mark_timing!("package_file_graph");
    ownership::importable::promote_importable_ownership_matches(
        rows,
        package_sources,
        &mut package_report,
    );
    mark_timing!("importable_promote");
    let matched_package_names = package_filter
        .cloned()
        .unwrap_or_else(|| unmatched_package_scope(rows));
    ownership::force_externalize::force_externalize_remaining_package_modules(
        rows,
        package_sources,
        &matched_package_names,
        &mut package_report,
    );
    mark_timing!("force_externalize");
    if timing_enabled {
        let _ = timing_last;
    }

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
    excluded_modules: &BTreeSet<ModuleId>,
    only_weak_package_sources: bool,
) -> BTreeMap<ModuleId, Vec<reverts_ir::FunctionFingerprint>> {
    let mut out = BTreeMap::new();
    for module in &rows.modules {
        if excluded_modules.contains(&module.id) {
            continue;
        }
        if module.kind != ModuleKind::Package {
            continue;
        }
        if let Some(package_filter) = package_filter
            && !module
                .package_name
                .as_deref()
                .is_some_and(|package_name| package_filter.contains(package_name))
        {
            continue;
        }
        if let Some(slice) = rows.module_source_slice(module.id) {
            let quality =
                package_module_source_quality(module, slice.source_file_path, slice.source);
            if quality == PackageModuleSourceQuality::Invalid
                || (only_weak_package_sources && quality != PackageModuleSourceQuality::Weak)
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
        let mut scoped_sources = package_sources
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
        if package_sources.len() > CASCADE_MATCHED_MODULE_SOURCE_LIMIT
            && scoped_sources.len() > CASCADE_SOURCE_GROUP_LIMIT
        {
            scoped_sources.retain(|source| source.external_importable);
        }
        if scoped_sources.len() > CASCADE_SOURCE_GROUP_LIMIT {
            continue;
        }
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

/// Pass-local memoization for external-import proof strategies.
///
/// This is scratch state for proof execution, not a package-source index and
/// not a policy object. Keeping it named and scoped as scratch makes the
/// mechanism/strategy boundary explicit: callers may reuse expensive facts
/// while each proof strategy still owns its own allow/score decisions.
#[derive(Debug, Default)]
pub(crate) struct ExternalImportProofScratch<'a> {
    module_fingerprints: RefCell<BTreeMap<ModuleId, Option<ModuleMatchFingerprint>>>,
    source_fingerprints_by_version:
        RefCell<BTreeMap<(String, String), Vec<PackageSourceFingerprint<'a>>>>,
    source_fingerprints_by_package: RefCell<BTreeMap<String, Vec<PackageSourceFingerprint<'a>>>>,
    dependency_graph_evidence:
        RefCell<BTreeMap<(ModuleId, String, String), DependencyGraphEvidence>>,
    direct_dependencies: RefCell<BTreeMap<ModuleId, Vec<ModuleId>>>,
    direct_dependents: RefCell<BTreeMap<ModuleId, Vec<ModuleId>>>,
    package_modules_by_id: RefCell<Option<BTreeMap<ModuleId, (String, String)>>>,
}

impl<'a> ExternalImportProofScratch<'a> {
    fn module_fingerprint(
        &self,
        module: &ModuleInput,
        path: &str,
        source: &str,
    ) -> Option<ModuleMatchFingerprint> {
        if let Some(fingerprint) = self.module_fingerprints.borrow().get(&module.id) {
            return fingerprint.clone();
        }
        let fingerprint = module_match_fingerprint(module, path, source).ok();
        self.module_fingerprints
            .borrow_mut()
            .insert(module.id, fingerprint.clone());
        fingerprint
    }

    fn source_fingerprints_for_version(
        &self,
        external_source_index: &ExternalImportSourceIndex<'a>,
        package_name: &str,
        package_version: &str,
    ) -> Vec<PackageSourceFingerprint<'a>> {
        let key = (package_name.to_string(), package_version.to_string());
        if let Some(fingerprints) = self.source_fingerprints_by_version.borrow().get(&key) {
            return fingerprints.clone();
        }
        let fingerprints = external_source_index
            .all_sources(package_name, package_version)
            .iter()
            .filter(|source| source.is_within_fingerprint_budget())
            .filter_map(|source| external_source_index.source_fingerprint(source))
            .collect::<Vec<_>>();
        self.source_fingerprints_by_version
            .borrow_mut()
            .insert(key, fingerprints.clone());
        fingerprints
    }

    fn source_fingerprints_for_package(
        &self,
        external_source_index: &ExternalImportSourceIndex<'a>,
        package_name: &str,
    ) -> Vec<PackageSourceFingerprint<'a>> {
        if let Some(fingerprints) = self
            .source_fingerprints_by_package
            .borrow()
            .get(package_name)
        {
            return fingerprints.clone();
        }
        let fingerprints = external_source_index
            .all_sources_for_package(package_name)
            .into_iter()
            .filter(|source| source.is_within_fingerprint_budget())
            .filter_map(|source| external_source_index.source_fingerprint(source))
            .collect::<Vec<_>>();
        self.source_fingerprints_by_package
            .borrow_mut()
            .insert(package_name.to_string(), fingerprints.clone());
        fingerprints
    }

    fn dependency_graph_evidence(
        &self,
        rows: &InputRows,
        module_id: ModuleId,
        candidate: &PackageSource,
        external_source_index: &ExternalImportSourceIndex<'a>,
        concrete_sources_by_module: &BTreeMap<ModuleId, ConcretePackageSourcePath>,
    ) -> DependencyGraphEvidence {
        let dependency_ids = self.direct_dependencies(rows, module_id);
        let dependent_ids = self.direct_dependents(rows, module_id);
        let neighborhood_signature = dependency_graph_concrete_neighborhood_signature(
            dependency_ids.as_slice(),
            dependent_ids.as_slice(),
            candidate,
            concrete_sources_by_module,
        );
        let key = (
            module_id,
            package_source_cache_key(candidate),
            neighborhood_signature,
        );
        if let Some(evidence) = self.dependency_graph_evidence.borrow().get(&key) {
            return *evidence;
        }
        let evidence = dependency_graph_source_evidence(
            candidate,
            external_source_index,
            concrete_sources_by_module,
            dependency_ids.as_slice(),
            dependent_ids.as_slice(),
        );
        self.dependency_graph_evidence
            .borrow_mut()
            .insert(key, evidence);
        evidence
    }

    fn direct_dependencies(&self, rows: &InputRows, module_id: ModuleId) -> Vec<ModuleId> {
        if let Some(dependencies) = self.direct_dependencies.borrow().get(&module_id) {
            return dependencies.clone();
        }
        let dependencies = direct_module_dependencies(rows, module_id);
        self.direct_dependencies
            .borrow_mut()
            .insert(module_id, dependencies.clone());
        dependencies
    }

    fn direct_dependents(&self, rows: &InputRows, module_id: ModuleId) -> Vec<ModuleId> {
        if let Some(dependents) = self.direct_dependents.borrow().get(&module_id) {
            return dependents.clone();
        }
        let dependents = direct_module_dependents(rows, module_id);
        self.direct_dependents
            .borrow_mut()
            .insert(module_id, dependents.clone());
        dependents
    }

    fn row_module_is_same_package_version(
        &self,
        rows: &InputRows,
        module_id: ModuleId,
        package_name: &str,
        package_version: &str,
    ) -> bool {
        if self.package_modules_by_id.borrow().is_none() {
            let package_modules = rows
                .modules
                .iter()
                .filter(|module| module.kind == ModuleKind::Package)
                .filter_map(|module| {
                    Some((
                        module.id,
                        (
                            module.package_name.as_deref()?.to_string(),
                            module.package_version.as_deref()?.to_string(),
                        ),
                    ))
                })
                .collect::<BTreeMap<_, _>>();
            *self.package_modules_by_id.borrow_mut() = Some(package_modules);
        }
        self.package_modules_by_id
            .borrow()
            .as_ref()
            .and_then(|package_modules| package_modules.get(&module_id))
            .is_some_and(|(name, version)| name == package_name && version == package_version)
    }
}

pub(crate) fn importable_package_source_for_module(
    module: &ModuleInput,
    package_match: &PackageMatch,
    external_source_index: &ExternalImportSourceIndex<'_>,
    module_source: &str,
) -> Option<ExternalImportTarget> {
    resolve_external_import_target_with_index(
        module,
        package_match.package_name.as_str(),
        package_match.package_version.as_str(),
        Some(package_match),
        external_source_index,
        module_source,
    )
}

#[cfg(test)]
fn resolve_external_import_target(
    module: &ModuleInput,
    package_name: &str,
    package_version: &str,
    package_match: Option<&PackageMatch>,
    package_sources: &[PackageSource],
    module_source: &str,
) -> Option<ExternalImportTarget> {
    let external_source_index = ExternalImportSourceIndex::build(package_sources);
    resolve_external_import_target_with_index(
        module,
        package_name,
        package_version,
        package_match,
        &external_source_index,
        module_source,
    )
}

fn resolve_external_import_target_with_index(
    module: &ModuleInput,
    package_name: &str,
    package_version: &str,
    package_match: Option<&PackageMatch>,
    external_source_index: &ExternalImportSourceIndex<'_>,
    module_source: &str,
) -> Option<ExternalImportTarget> {
    let cache = ExternalImportProofScratch::default();
    if let Some(target) = normalized_source_external_package_source(
        module,
        package_name,
        package_version,
        external_source_index,
        module_source,
    ) {
        return Some(target);
    }

    if let Some(package_match) = package_match
        && let Some(target) =
            exact_importable_package_match_source(package_match, external_source_index)
    {
        return Some(target);
    }

    if let Some(package_match) = package_match
        && let Some(target) = dependency_exact_hint_source_match_external_package_source(
            module,
            package_match,
            external_source_index,
            module_source,
            &cache,
        )
    {
        return Some(target);
    }

    if let Some(package_match) = package_match
        && let Some(target) = export_member_external_package_source(
            package_match,
            external_source_index,
            module_source,
        )
    {
        return Some(target);
    }

    if let Some(package_match) = package_match
        && let Some(target) = canonical_subpath_external_package_source(
            module,
            package_match,
            external_source_index,
            module_source,
        )
    {
        return Some(target);
    }

    if let Some(package_match) = package_match
        && let Some(target) = public_export_member_external_package_source(
            module,
            package_match,
            external_source_index,
            module_source,
        )
    {
        return Some(target);
    }

    let semantic_policies = package_match
        .map(semantic_external_target_policies)
        .unwrap_or_default();
    if semantic_policies.is_empty() {
        return None;
    }

    for semantic_policy in semantic_policies {
        let hints = semantic_external_target_hints(
            package_name,
            module.semantic_path.as_str(),
            package_match,
            module_source,
            semantic_policy,
        );
        if let Some(target) = semantic_external_package_source(
            package_name,
            package_version,
            external_source_index,
            hints.as_slice(),
            semantic_policy.min_score,
        ) {
            return Some(target);
        }
        if let Some(package_match) = package_match
            && let Some(target) = semantic_source_only_export_member_package_source(
                package_match,
                external_source_index,
                hints.as_slice(),
                semantic_policy.min_score,
                module_source,
            )
        {
            return Some(target);
        }
    }
    None
}

fn semantic_external_target_hints(
    package_name: &str,
    module_semantic_path: &str,
    package_match: Option<&PackageMatch>,
    module_source: &str,
    semantic_policy: SemanticExternalTargetPolicy,
) -> Vec<String> {
    let mut hints = module_package_semantic_path_hints(
        package_name,
        module_semantic_path,
        module_source,
        semantic_policy.hint_mode,
    );
    if let Some(package_match) = package_match
        && let Some(exact_path) = exact_hint_semantic_path(package_match.source_path.as_str())
    {
        hints.extend(module_package_semantic_path_hints(
            package_match.package_name.as_str(),
            exact_path.as_str(),
            module_source,
            semantic_policy.hint_mode,
        ));
        if let Some(hint) = trusted_exact_generated_filename_hint(
            package_match,
            exact_path.as_str(),
            semantic_policy.hint_mode,
        ) {
            hints.push(hint);
        }
    }
    hints.sort();
    hints.dedup();
    hints
}

fn dependency_exact_hint_source_match_external_package_source<'a>(
    module: &ModuleInput,
    package_match: &PackageMatch,
    external_source_index: &ExternalImportSourceIndex<'a>,
    module_source: &str,
    cache: &ExternalImportProofScratch<'a>,
) -> Option<ExternalImportTarget> {
    if package_match.strategy != ModuleMatchStrategy::DependencyClosureOwnership
        || !package_match.source_path.starts_with("exact-hint:")
        || !package_match.source_path.contains(":quality=trusted:")
        || module_source.trim().is_empty()
    {
        return None;
    }
    let module_fingerprint =
        cache.module_fingerprint(module, module.semantic_path.as_str(), module_source)?;
    let sources = cache.source_fingerprints_for_version(
        external_source_index,
        package_match.package_name.as_str(),
        package_match.package_version.as_str(),
    );
    if sources.is_empty() {
        return None;
    }
    let version = PackageVersionCandidate {
        package_name: package_match.package_name.clone(),
        package_version: package_match.package_version.clone(),
        sources,
    };
    let source_match = best_source_match(
        &version,
        &module_fingerprint,
        &VersionedPackageMatcherConfig::default(),
    )?;
    match source_match.strategy {
        ModuleMatchStrategy::NormalizedSourceHash
        | ModuleMatchStrategy::FunctionSignatureAndStringAnchors
        | ModuleMatchStrategy::PropertyShapeAndStringAnchors
        | ModuleMatchStrategy::ObjectShapeAndStringAnchors
        | ModuleMatchStrategy::ClassShapeAndStringAnchors
        | ModuleMatchStrategy::SwitchShapeAndStringAnchors => {}
        ModuleMatchStrategy::AggregateFunctionSignatureAndStringAnchors
        | ModuleMatchStrategy::CascadeFunctionCoverage
        | ModuleMatchStrategy::CascadeFunctionOwnership
        | ModuleMatchStrategy::CascadePartialFunctionCoverage
        | ModuleMatchStrategy::AggregateStructuralBagSimilarity
        | ModuleMatchStrategy::DependencyClosureOwnership => return None,
    }
    if source_match.external_importable {
        return Some(ExternalImportTarget {
            export_specifier: source_match.export_specifier,
            source_path: format!("forced-external:source-match:{}", source_match.source_path),
        });
    }
    export_member_external_package_source_for_source_path(
        source_match.package_name.as_str(),
        source_match.package_version.as_str(),
        source_match.source_path.as_str(),
        external_source_index,
        module_source,
    )
}

fn semantic_external_package_source(
    package_name: &str,
    package_version: &str,
    external_source_index: &ExternalImportSourceIndex<'_>,
    hints: &[String],
    min_score: usize,
) -> Option<ExternalImportTarget> {
    if hints.is_empty() {
        return None;
    }
    let mut scored = external_source_index
        .sources(package_name, package_version)
        .iter()
        .copied()
        .filter_map(|source| {
            let (score, proof) = hints
                .iter()
                .map(|hint| semantic_external_source_score(source, hint))
                .max_by(|left, right| {
                    left.0.cmp(&right.0).then_with(|| {
                        semantic_external_source_proof_rank(left.1)
                            .cmp(&semantic_external_source_proof_rank(right.1))
                    })
                })
                .unwrap_or((0, SemanticExternalSourceProof::SourcePath));
            (score >= min_score).then_some((source, score, proof))
        })
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| {
                semantic_external_source_proof_rank(right.2)
                    .cmp(&semantic_external_source_proof_rank(left.2))
            })
            .then_with(|| left.0.export_specifier.cmp(&right.0.export_specifier))
            .then_with(|| left.0.source_path.cmp(&right.0.source_path))
    });
    let best_score = scored.first()?.1;
    let best_proof = scored.first()?.2;
    let best = scored
        .into_iter()
        .filter(|(_source, score, proof)| *score == best_score && *proof == best_proof)
        .map(|(source, _score, _proof)| source)
        .collect::<Vec<_>>();
    let export_specifiers = best
        .iter()
        .map(|source| source.export_specifier.as_str())
        .collect::<BTreeSet<_>>();
    if export_specifiers.len() != 1 {
        let source = disambiguate_semantic_build_variant_source(best.as_slice())?;
        return Some(ExternalImportTarget {
            export_specifier: source.export_specifier.clone(),
            source_path: format!(
                "forced-external:{}:build-variant:{}",
                semantic_external_source_proof_label(best_proof),
                source.source_path
            ),
        });
    }
    let export_specifier = export_specifiers.into_iter().next()?;
    let source = best.into_iter().min_by(|left, right| {
        package_source_external_import_rank(left)
            .cmp(&package_source_external_import_rank(right))
            .then_with(|| left.source_path.cmp(&right.source_path))
    })?;
    Some(ExternalImportTarget {
        export_specifier: export_specifier.to_string(),
        source_path: format!(
            "forced-external:{}:{}",
            semantic_external_source_proof_label(best_proof),
            source.source_path
        ),
    })
}

fn canonical_subpath_external_package_source(
    module: &ModuleInput,
    package_match: &PackageMatch,
    external_source_index: &ExternalImportSourceIndex<'_>,
    module_source: &str,
) -> Option<ExternalImportTarget> {
    if !canonical_subpath_policy_allows(package_match) {
        return None;
    }
    let mut hints = module_package_semantic_path_hints(
        package_match.package_name.as_str(),
        module.semantic_path.as_str(),
        module_source,
        SemanticPathHintMode::RelaxedImportProof,
    );
    if let Some(exact_hint) = exact_hint_semantic_path(package_match.source_path.as_str()) {
        hints.extend(module_package_semantic_path_hints(
            package_match.package_name.as_str(),
            exact_hint.as_str(),
            module_source,
            SemanticPathHintMode::RelaxedImportProof,
        ));
    }
    hints.sort();
    hints.dedup();
    let mut scored = external_source_index
        .sources(
            package_match.package_name.as_str(),
            package_match.package_version.as_str(),
        )
        .iter()
        .copied()
        .filter_map(|source| {
            let best_score = hints
                .iter()
                .map(|hint| package_source_semantic_surface_hint_score(source, hint))
                .max()
                .unwrap_or(0);
            (best_score >= 5).then_some((source, best_score))
        })
        .collect::<Vec<_>>();
    if scored.is_empty() {
        return None;
    }
    scored.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| {
                package_source_external_import_rank(left.0)
                    .cmp(&package_source_external_import_rank(right.0))
            })
            .then_with(|| left.0.export_specifier.cmp(&right.0.export_specifier))
            .then_with(|| left.0.source_path.cmp(&right.0.source_path))
    });
    let best_score = scored.first()?.1;
    let best = scored
        .into_iter()
        .filter(|(_source, score)| *score == best_score)
        .map(|(source, _score)| source)
        .collect::<Vec<_>>();
    let export_specifiers = best
        .iter()
        .map(|source| source.export_specifier.as_str())
        .collect::<BTreeSet<_>>();
    if export_specifiers.len() != 1 {
        return None;
    }
    let export_specifier = export_specifiers.into_iter().next()?;
    let source = best.into_iter().min_by(|left, right| {
        package_source_external_import_rank(left)
            .cmp(&package_source_external_import_rank(right))
            .then_with(|| left.source_path.cmp(&right.source_path))
    })?;
    Some(ExternalImportTarget {
        export_specifier: export_specifier.to_string(),
        source_path: format!("forced-external:canonical-subpath:{}", source.source_path),
    })
}

fn exact_hint_semantic_path(source_path: &str) -> Option<String> {
    source_path
        .split(":semantic_path=")
        .nth(1)
        .map(|tail| tail.split(':').next().unwrap_or(tail))
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(ToOwned::to_owned)
}

fn trusted_exact_generated_filename_hint(
    package_match: &PackageMatch,
    exact_semantic_path: &str,
    hint_mode: SemanticPathHintMode,
) -> Option<String> {
    if !matches!(
        hint_mode,
        SemanticPathHintMode::ImportProof | SemanticPathHintMode::RelaxedImportProof
    ) || !package_match.source_path.starts_with("exact-hint:")
        || !package_match.source_path.contains(":quality=trusted:")
    {
        return None;
    }
    let stem = exact_semantic_path
        .strip_prefix("modules/")
        .map(strip_source_extension)
        .map(str::trim)?;
    let (prefix, rest) = stem.split_once('-')?;
    if prefix.is_empty() || !prefix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let hint = rest.trim_matches('/').to_ascii_lowercase();
    if !generated_filename_hint_is_public_member_bridge_candidate(hint.as_str()) {
        return None;
    }
    Some(hint)
}

fn generated_filename_hint_is_public_member_bridge_candidate(hint: &str) -> bool {
    let trimmed = hint.trim().trim_matches('/');
    if trimmed.is_empty() || trimmed.contains('/') {
        return false;
    }
    let tokens = path_hint_tokens(trimmed);
    if tokens.len() < 2 {
        return false;
    }
    if tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            "init"
                | "internal"
                | "internals"
                | "wrapper"
                | "runtime"
                | "deps"
                | "dependency"
                | "dependencies"
                | "helper"
                | "helpers"
                | "util"
                | "utils"
        )
    }) {
        return false;
    }
    tokens
        .iter()
        .any(|token| token.len() >= 4 && !is_build_path_segment(token.as_str()))
}

fn disambiguate_semantic_build_variant_source<'a>(
    sources: &[&'a PackageSource],
) -> Option<&'a PackageSource> {
    if sources.is_empty() {
        return None;
    }
    let source_keys = sources
        .iter()
        .map(|source| semantic_build_variant_key(package_source_relative_path(source).as_str()))
        .collect::<BTreeSet<_>>();
    let export_keys = sources
        .iter()
        .map(|source| semantic_build_variant_key(package_source_export_path(source).as_str()))
        .collect::<BTreeSet<_>>();
    let source_key = source_keys.iter().next()?;
    if source_keys.len() != 1 || source_key.is_empty() || export_keys.len() != 1 {
        return None;
    }

    let best_rank = sources
        .iter()
        .map(|source| package_source_external_import_rank(source))
        .min()?;
    let best = sources
        .iter()
        .copied()
        .filter(|source| package_source_external_import_rank(source) == best_rank)
        .collect::<Vec<_>>();
    (best.len() == 1).then_some(best[0])
}

fn semantic_build_variant_key(path: &str) -> Vec<String> {
    canonical_public_path_segments(path)
}

fn semantic_source_only_export_member_package_source(
    package_match: &PackageMatch,
    external_source_index: &ExternalImportSourceIndex<'_>,
    hints: &[String],
    min_score: usize,
    module_source: &str,
) -> Option<ExternalImportTarget> {
    if hints.is_empty()
        || !semantic_source_only_export_member_policy_allows(package_match)
        || module_source.trim().is_empty()
    {
        return None;
    }
    // Importable sources were already handled by semantic_external_package_source.
    // For source-only package files, require at least a structured suffix/path
    // match and then separately prove that a public import surface re-exports
    // the matched members.
    let min_score = if package_match.source_path.contains(":quality=trusted:") && min_score <= 1 {
        3
    } else {
        min_score.max(4)
    };
    let mut scored = external_source_index
        .all_sources(
            package_match.package_name.as_str(),
            package_match.package_version.as_str(),
        )
        .iter()
        .copied()
        .filter(|source| !source.external_importable)
        .filter_map(|source| {
            let export_members = external_source_index.export_members(source);
            let (score, proof) = hints
                .iter()
                .map(|hint| {
                    semantic_source_only_external_source_score(source, &export_members, hint)
                })
                .max_by(|left, right| {
                    left.0.cmp(&right.0).then_with(|| {
                        semantic_external_source_proof_rank(left.1)
                            .cmp(&semantic_external_source_proof_rank(right.1))
                    })
                })
                .unwrap_or((0, SemanticExternalSourceProof::SourcePath));
            (score >= min_score).then_some((source, score, proof))
        })
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| {
                semantic_external_source_proof_rank(right.2)
                    .cmp(&semantic_external_source_proof_rank(left.2))
            })
            .then_with(|| left.0.source_path.cmp(&right.0.source_path))
            .then_with(|| left.0.export_specifier.cmp(&right.0.export_specifier))
    });
    let best_score = scored.first()?.1;
    let best_proof = scored.first()?.2;
    let best = scored
        .into_iter()
        .filter(|(_source, score, proof)| *score == best_score && *proof == best_proof)
        .map(|(source, _score, _proof)| source)
        .collect::<Vec<_>>();
    let targets = best
        .into_iter()
        .filter_map(|source| {
            export_member_external_package_source_for_source_path(
                package_match.package_name.as_str(),
                package_match.package_version.as_str(),
                source.source_path.as_str(),
                external_source_index,
                module_source,
            )
        })
        .map(|target| (target.export_specifier, target.source_path))
        .collect::<BTreeSet<_>>();
    if targets.len() != 1 {
        return None;
    }
    let (export_specifier, source_path) = targets.into_iter().next()?;
    Some(ExternalImportTarget {
        export_specifier,
        source_path,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SemanticExternalSourceProof {
    SourcePath,
    ExportSurface,
    ExportMember,
}

fn semantic_external_source_score(
    source: &PackageSource,
    hint: &str,
) -> (usize, SemanticExternalSourceProof) {
    let source_score =
        package_source_semantic_hint_score(package_source_relative_path(source).as_str(), hint);
    let export_score =
        package_source_semantic_hint_score(package_source_export_path(source).as_str(), hint);
    if export_score > source_score {
        (export_score, SemanticExternalSourceProof::ExportSurface)
    } else {
        (source_score, SemanticExternalSourceProof::SourcePath)
    }
}

fn semantic_source_only_external_source_score(
    source: &PackageSource,
    export_members: &BTreeSet<String>,
    hint: &str,
) -> (usize, SemanticExternalSourceProof) {
    let (path_score, path_proof) = semantic_external_source_score(source, hint);
    let member_score = if semantic_export_member_hint_source_is_narrow(source, export_members) {
        semantic_export_member_hint_score(export_members, hint)
    } else {
        0
    };
    if member_score > path_score
        || (member_score == path_score
            && member_score > 0
            && semantic_external_source_proof_rank(SemanticExternalSourceProof::ExportMember)
                > semantic_external_source_proof_rank(path_proof))
    {
        (member_score, SemanticExternalSourceProof::ExportMember)
    } else {
        (path_score, path_proof)
    }
}

fn semantic_export_member_hint_source_is_narrow(
    source: &PackageSource,
    export_members: &BTreeSet<String>,
) -> bool {
    if export_members.is_empty() || export_members.len() > 8 {
        return false;
    }
    let relative_path = package_source_relative_path(source);
    let leaf = strip_source_extension(relative_path.as_str())
        .trim_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    !matches!(leaf, "" | "index")
}

fn semantic_export_member_hint_score(export_members: &BTreeSet<String>, hint: &str) -> usize {
    let hint = hint.trim().trim_matches('/');
    if hint.is_empty() {
        return 0;
    }
    let hint_last_segment = hint.rsplit('/').next().unwrap_or(hint);
    let hint_last_normalized = normalize_hint_text(hint_last_segment);
    if hint_last_normalized.len() < 4 {
        return 0;
    }
    let hint_tokens = path_hint_tokens(hint_last_segment);
    export_members
        .iter()
        .filter_map(|member| {
            let member_normalized = normalize_hint_text(member);
            if member_normalized.len() < 4 {
                return None;
            }
            if member_normalized == hint_last_normalized {
                return Some(3);
            }
            let member_tokens = path_hint_tokens(member);
            if hint_tokens.len() >= 2
                && !member_tokens.is_empty()
                && hint_tokens
                    .iter()
                    .all(|token| member_tokens.contains(token))
            {
                return Some(3);
            }
            None
        })
        .max()
        .unwrap_or(0)
}

fn exact_importable_package_match_source(
    package_match: &PackageMatch,
    external_source_index: &ExternalImportSourceIndex<'_>,
) -> Option<ExternalImportTarget> {
    if package_match.strategy != ModuleMatchStrategy::NormalizedSourceHash
        || package_match.normalized_source_hash.trim().is_empty()
    {
        return None;
    }
    let sources = external_source_index.normalized_sources(
        package_match.package_name.as_str(),
        package_match.package_version.as_str(),
        package_match.normalized_source_hash.as_str(),
    );
    let exact_source_paths = sources
        .iter()
        .copied()
        .filter(|source| source.source_path == package_match.source_path)
        .map(|source| {
            (
                source.export_specifier.as_str(),
                source.source_path.as_str(),
            )
        })
        .collect::<BTreeSet<_>>();
    if exact_source_paths.len() == 1 {
        let (export_specifier, source_path) = exact_source_paths.into_iter().next()?;
        return Some(ExternalImportTarget {
            export_specifier: export_specifier.to_string(),
            source_path: source_path.to_string(),
        });
    }
    None
}

fn normalized_source_external_package_source(
    module: &ModuleInput,
    package_name: &str,
    package_version: &str,
    external_source_index: &ExternalImportSourceIndex<'_>,
    module_source: &str,
) -> Option<ExternalImportTarget> {
    if module_source.trim().is_empty() {
        return None;
    }
    let normalized = normalize_source(module.semantic_path.as_str(), module_source).ok()?;
    let normalized_hash = stable_hash(normalized.as_bytes());
    let candidates = external_source_index.normalized_sources(
        package_name,
        package_version,
        normalized_hash.as_str(),
    );
    let best = *candidates.first()?;
    let best_key = package_source_external_import_rank(best);
    if candidates.get(1).is_some_and(|candidate| {
        package_source_external_import_rank(candidate) == best_key
            && candidate.export_specifier != best.export_specifier
    }) {
        return None;
    }
    Some(ExternalImportTarget {
        export_specifier: best.export_specifier.clone(),
        source_path: format!("normalized-source-export:{}", best.source_path),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ExportMemberSourceProof {
    BarrelReference,
    BuildVariantPeer,
    CommonJsReexport,
    ExportAllReexport,
    NamedReexport,
    SourceEquivalent,
}

#[derive(Debug, Clone, Copy)]
struct ExportMemberExternalCandidate<'a> {
    external: &'a PackageSource,
    matched: &'a PackageSource,
    proof: ExportMemberSourceProof,
}

fn export_member_external_package_source(
    package_match: &PackageMatch,
    external_source_index: &ExternalImportSourceIndex<'_>,
    module_source: &str,
) -> Option<ExternalImportTarget> {
    if !source_only_match_can_be_promoted_to_import(package_match.strategy) {
        return None;
    }
    export_member_external_package_source_for_source_path(
        package_match.package_name.as_str(),
        package_match.package_version.as_str(),
        package_match.source_path.as_str(),
        external_source_index,
        module_source,
    )
}

fn public_export_member_external_package_source(
    module: &ModuleInput,
    package_match: &PackageMatch,
    external_source_index: &ExternalImportSourceIndex<'_>,
    module_source: &str,
) -> Option<ExternalImportTarget> {
    if module_source.trim().is_empty() || !public_export_member_policy_allows(package_match) {
        return None;
    }
    let module_members = exported_members_from_source(module.semantic_path.as_str(), module_source);
    if !export_member_set_is_strong(module_members.iter()) {
        return None;
    }
    let semantic_policies = semantic_external_target_policies(package_match);
    if semantic_policies.is_empty() {
        return None;
    }
    let mut candidates = external_source_index
        .sources(
            package_match.package_name.as_str(),
            package_match.package_version.as_str(),
        )
        .iter()
        .copied()
        .filter(|source| {
            let external_members = external_source_index.export_members(source);
            module_members.is_subset(&external_members)
                && export_member_set_is_strong(external_members.iter())
        })
        .filter_map(|source| {
            let best_semantic_score = semantic_policies
                .iter()
                .filter_map(|policy| {
                    let hints = module_package_semantic_path_hints(
                        package_match.package_name.as_str(),
                        module.semantic_path.as_str(),
                        module_source,
                        policy.hint_mode,
                    );
                    hints
                        .iter()
                        .map(|hint| semantic_external_source_score(source, hint).0)
                        .max()
                        .filter(|score| *score >= policy.min_score)
                })
                .max()
                .unwrap_or(0);
            let public_member_score = public_export_member_signature_score(
                module_source,
                source.source.as_str(),
                &module_members,
            );
            (best_semantic_score > 0 || public_member_score > 0).then_some((
                source,
                best_semantic_score,
                public_member_score,
            ))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .2
            .cmp(&left.2)
            .then_with(|| right.1.cmp(&left.1))
            .then_with(|| {
                package_source_external_import_rank(left.0)
                    .cmp(&package_source_external_import_rank(right.0))
            })
            .then_with(|| left.0.export_specifier.cmp(&right.0.export_specifier))
            .then_with(|| left.0.source_path.cmp(&right.0.source_path))
    });
    let best_signature_score = candidates.first()?.2;
    let best_semantic_score = candidates.first()?.1;
    let best_rank = package_source_external_import_rank(candidates.first()?.0);
    let best = candidates
        .into_iter()
        .filter(|(source, semantic_score, signature_score)| {
            *signature_score == best_signature_score
                && *semantic_score == best_semantic_score
                && package_source_external_import_rank(source) == best_rank
        })
        .collect::<Vec<_>>();
    let export_specifiers = best
        .iter()
        .map(|(source, _, _)| source.export_specifier.as_str())
        .collect::<BTreeSet<_>>();
    if export_specifiers.len() != 1 {
        return None;
    }
    let export_specifier = export_specifiers.into_iter().next()?;
    let source = best.into_iter().min_by(|left, right| {
        package_source_external_import_rank(left.0)
            .cmp(&package_source_external_import_rank(right.0))
            .then_with(|| left.0.source_path.cmp(&right.0.source_path))
    })?;
    Some(ExternalImportTarget {
        export_specifier: export_specifier.to_string(),
        source_path: format!(
            "forced-external:public-export-members:members={}:{}",
            export_member_proof_fragment(&module_members),
            source.0.source_path
        ),
    })
}

fn public_export_member_signature_score(
    module_source: &str,
    external_source: &str,
    module_members: &BTreeSet<String>,
) -> usize {
    let module_signatures = binding_string_signatures_from_source(module_source);
    let external_signatures = binding_string_signatures_from_source(external_source);
    module_members
        .iter()
        .filter_map(|member| {
            let module_signature = module_signatures.get(member)?;
            let external_signature = external_signatures.get(member)?;
            let overlap = module_signature.intersection(external_signature).count();
            if overlap > 0 {
                return Some(1_000 + overlap);
            }
            export_member_alias_score(module_signature, member, external_signature)
        })
        .max()
        .unwrap_or(0)
}

fn export_member_external_package_source_for_source_path(
    package_name: &str,
    package_version: &str,
    matched_source_path: &str,
    external_source_index: &ExternalImportSourceIndex<'_>,
    module_source: &str,
) -> Option<ExternalImportTarget> {
    let matched_sources = external_source_index.all_sources_by_path(
        package_name,
        package_version,
        matched_source_path,
    );
    if matched_sources.is_empty() {
        return None;
    }

    let matched_members = matched_sources
        .iter()
        .flat_map(|source| external_source_index.export_members(source))
        .filter(|member| is_usable_export_member(member))
        .collect::<BTreeSet<_>>();
    if !export_member_set_is_strong(matched_members.iter()) {
        return None;
    }

    let mut candidates = Vec::<ExportMemberExternalCandidate<'_>>::new();
    for external in external_source_index.sources(package_name, package_version) {
        let external_members = external_source_index.export_members(external);
        let Some((matched, proof)) = matched_sources
            .iter()
            .filter_map(|matched| {
                let proof = export_member_source_proof(
                    matched,
                    external,
                    &matched_members,
                    &external_members,
                    external_source_index,
                )?;
                Some((*matched, proof))
            })
            .max_by(|left, right| {
                export_member_source_proof_rank(left.1)
                    .cmp(&export_member_source_proof_rank(right.1))
            })
        else {
            continue;
        };
        candidates.push(ExportMemberExternalCandidate {
            external,
            matched,
            proof,
        });
    }
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by(|left, right| {
        export_member_source_proof_rank(right.proof)
            .cmp(&export_member_source_proof_rank(left.proof))
            .then_with(|| {
                package_source_external_import_rank(left.external)
                    .cmp(&package_source_external_import_rank(right.external))
            })
            .then_with(|| {
                left.external
                    .export_specifier
                    .cmp(&right.external.export_specifier)
            })
            .then_with(|| left.external.source_path.cmp(&right.external.source_path))
            .then_with(|| left.matched.source_path.cmp(&right.matched.source_path))
    });
    let best_proof = candidates.first()?.proof;
    let best_rank = package_source_external_import_rank(candidates.first()?.external);
    let best = candidates
        .into_iter()
        .filter(|candidate| {
            candidate.proof == best_proof
                && package_source_external_import_rank(candidate.external) == best_rank
        })
        .collect::<Vec<_>>();
    let export_specifiers = best
        .iter()
        .map(|candidate| candidate.external.export_specifier.as_str())
        .collect::<BTreeSet<_>>();
    if export_specifiers.len() != 1 {
        return None;
    }
    let export_specifier = export_specifiers.into_iter().next()?;
    let source = best.into_iter().min_by(|left, right| {
        package_source_external_import_rank(left.external)
            .cmp(&package_source_external_import_rank(right.external))
            .then_with(|| left.external.source_path.cmp(&right.external.source_path))
            .then_with(|| left.matched.source_path.cmp(&right.matched.source_path))
    })?;
    let alias_source = if export_member_source_proof_alias_source_is_matched(best_proof) {
        source.matched
    } else {
        source.external
    };
    let alias_members = if export_member_source_proof_alias_source_is_matched(best_proof) {
        matched_members.clone()
    } else {
        external_source_index.export_members(alias_source)
    };
    let aliases =
        export_member_alias_proof_map(module_source, alias_source.source.as_str(), &alias_members);
    Some(ExternalImportTarget {
        export_specifier: export_specifier.to_string(),
        source_path: export_member_proof_source_path(
            source.external,
            best_proof,
            &matched_members,
            &aliases,
        ),
    })
}

fn export_member_source_proof(
    matched: &PackageSource,
    external: &PackageSource,
    matched_members: &BTreeSet<String>,
    external_members: &BTreeSet<String>,
    external_source_index: &ExternalImportSourceIndex<'_>,
) -> Option<ExportMemberSourceProof> {
    if package_sources_are_equivalent(matched, external) {
        return Some(ExportMemberSourceProof::SourceEquivalent);
    }
    if export_member_build_variant_peer(matched, external)
        && matched_members == external_members
        && export_member_set_is_strong(matched_members.iter())
    {
        return Some(ExportMemberSourceProof::BuildVariantPeer);
    }
    if matched_members.is_subset(external_members)
        && external_source_references_matched_member_source(external, matched)
    {
        return Some(ExportMemberSourceProof::BarrelReference);
    }
    if external_source_commonjs_reexports_matched_source(external, matched) {
        return Some(ExportMemberSourceProof::CommonJsReexport);
    }
    if external_source_export_all_reexports_matched_source(external, matched) {
        return Some(ExportMemberSourceProof::ExportAllReexport);
    }
    if external_source_export_all_reexports_matched_source_transitively(
        external,
        matched,
        external_source_index,
    ) {
        return Some(ExportMemberSourceProof::ExportAllReexport);
    }
    if external_source_reexports_matched_source_transitively(
        external,
        matched,
        external_source_index,
    ) {
        return Some(ExportMemberSourceProof::NamedReexport);
    }
    None
}

fn package_sources_are_equivalent(left: &PackageSource, right: &PackageSource) -> bool {
    if left.source == right.source {
        return true;
    }
    if let (Ok(left_normalized), Ok(right_normalized)) = (
        normalize_source(left.source_path.as_str(), left.source.as_str()),
        normalize_source(right.source_path.as_str(), right.source.as_str()),
    ) && stable_hash(left_normalized.as_bytes()) == stable_hash(right_normalized.as_bytes())
    {
        return true;
    }
    if left.source.len() > PACKAGE_SOURCE_FINGERPRINT_MAX_BYTES
        || right.source.len() > PACKAGE_SOURCE_FINGERPRINT_MAX_BYTES
    {
        return false;
    }
    let (Ok(left_fingerprint), Ok(right_fingerprint)) = (
        fingerprint_source(left.source_path.as_str(), left.source.as_str()),
        fingerprint_source(right.source_path.as_str(), right.source.as_str()),
    ) else {
        return false;
    };
    let function_matches = left_fingerprint
        .function_signature_hashes
        .intersection(&right_fingerprint.function_signature_hashes)
        .count();
    let string_matches = left_fingerprint
        .string_anchors
        .intersection(&right_fingerprint.string_anchors)
        .count();
    function_matches >= 3 || (function_matches >= 2 && string_matches >= 1)
}

fn export_member_build_variant_peer(left: &PackageSource, right: &PackageSource) -> bool {
    package_source_variant_neutral_path(left) == package_source_variant_neutral_path(right)
}

fn package_source_variant_neutral_path(source: &PackageSource) -> String {
    let entry = strip_source_extension(package_source_entry_path(source).as_str())
        .trim_matches('/')
        .to_ascii_lowercase();
    entry
        .split('/')
        .filter(|segment| {
            !matches!(
                *segment,
                "dist-cjs"
                    | "dist-es"
                    | "dist-esm"
                    | "cjs"
                    | "commonjs"
                    | "esm"
                    | "es"
                    | "module"
                    | "modules"
            )
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn external_source_references_matched_member_source(
    external: &PackageSource,
    matched: &PackageSource,
) -> bool {
    let external_source = external.source.replace('\\', "/").to_ascii_lowercase();
    let matched_entry = strip_source_extension(package_source_entry_path(matched).as_str())
        .trim_matches('/')
        .to_ascii_lowercase();
    let leaf = matched_entry
        .rsplit('/')
        .next()
        .unwrap_or(matched_entry.as_str());
    let mut candidates = BTreeSet::new();
    if is_strong_path_hint_token(leaf) {
        candidates.insert(leaf.to_string());
    }
    let tail = matched_entry
        .rsplit('/')
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("/");
    if tail.len() >= 4 {
        candidates.insert(tail);
    }
    if matched_entry.len() >= 4 {
        candidates.insert(matched_entry);
    }
    candidates.into_iter().any(|candidate| {
        external_source_contains_path_reference(external_source.as_str(), candidate.as_str())
    })
}

fn external_source_contains_path_reference(source: &str, candidate: &str) -> bool {
    source.contains(format!("./{candidate}").as_str())
        || source.contains(format!("../{candidate}").as_str())
        || source.contains(format!("/{candidate}").as_str())
        || (candidate.contains('/') && source.contains(candidate))
}

fn external_source_commonjs_reexports_matched_source(
    external: &PackageSource,
    matched: &PackageSource,
) -> bool {
    commonjs_reexport_targets(external.source.as_str())
        .into_iter()
        .any(|target| relative_require_targets_package_source(external, target.as_str(), matched))
}

fn external_source_export_all_reexports_matched_source(
    external: &PackageSource,
    matched: &PackageSource,
) -> bool {
    export_all_reexport_targets(external.source.as_str())
        .into_iter()
        .any(|target| relative_require_targets_package_source(external, target.as_str(), matched))
}

fn external_source_export_all_reexports_matched_source_transitively(
    external: &PackageSource,
    matched: &PackageSource,
    external_source_index: &ExternalImportSourceIndex<'_>,
) -> bool {
    let matched_entry = package_source_entry_path(matched);
    let mut visited = BTreeSet::<String>::new();
    external_source_export_all_reexports_entry_transitively(
        external,
        matched_entry.as_str(),
        external_source_index,
        &mut visited,
    )
}

fn external_source_export_all_reexports_entry_transitively(
    source: &PackageSource,
    matched_entry: &str,
    external_source_index: &ExternalImportSourceIndex<'_>,
    visited: &mut BTreeSet<String>,
) -> bool {
    let source_key = format!(
        "{}@{}:{}",
        source.package_name, source.package_version, source.source_path
    );
    if !visited.insert(source_key) {
        return false;
    }
    for entry in package_source_export_all_reexport_entries(source) {
        if source_entry_paths_match(entry.as_str(), matched_entry) {
            return true;
        }
        for next in sources_matching_entry(
            source.package_name.as_str(),
            source.package_version.as_str(),
            entry.as_str(),
            external_source_index,
        ) {
            if external_source_export_all_reexports_entry_transitively(
                next,
                matched_entry,
                external_source_index,
                visited,
            ) {
                return true;
            }
        }
    }
    false
}

fn package_source_export_all_reexport_entries(source: &PackageSource) -> BTreeSet<String> {
    export_all_reexport_targets(source.source.as_str())
        .into_iter()
        .filter_map(|target| {
            resolve_package_relative_require(package_source_entry_path(source).as_str(), &target)
        })
        .map(|entry| {
            strip_source_extension(entry.as_str())
                .trim_matches('/')
                .to_ascii_lowercase()
        })
        .filter(|entry| !entry.is_empty())
        .collect()
}

fn external_source_reexports_matched_source_transitively(
    external: &PackageSource,
    matched: &PackageSource,
    external_source_index: &ExternalImportSourceIndex<'_>,
) -> bool {
    let matched_entry = package_source_entry_path(matched);
    let mut visited = BTreeSet::<String>::new();
    external_source_reexports_entry_transitively(
        external,
        matched_entry.as_str(),
        external_source_index,
        &mut visited,
    )
}

fn external_source_reexports_entry_transitively(
    source: &PackageSource,
    matched_entry: &str,
    external_source_index: &ExternalImportSourceIndex<'_>,
    visited: &mut BTreeSet<String>,
) -> bool {
    let source_key = format!(
        "{}@{}:{}",
        source.package_name, source.package_version, source.source_path
    );
    if !visited.insert(source_key) {
        return false;
    }
    for entry in package_source_reexport_entries(source) {
        if source_entry_paths_match(entry.as_str(), matched_entry) {
            return true;
        }
        for next in sources_matching_entry(
            source.package_name.as_str(),
            source.package_version.as_str(),
            entry.as_str(),
            external_source_index,
        ) {
            if external_source_reexports_entry_transitively(
                next,
                matched_entry,
                external_source_index,
                visited,
            ) {
                return true;
            }
        }
    }
    false
}

fn package_source_reexport_entries(source: &PackageSource) -> BTreeSet<String> {
    reexport_targets(source.source.as_str())
        .into_iter()
        .filter_map(|target| {
            resolve_package_relative_require(package_source_entry_path(source).as_str(), &target)
        })
        .map(|entry| {
            strip_source_extension(entry.as_str())
                .trim_matches('/')
                .to_ascii_lowercase()
        })
        .filter(|entry| !entry.is_empty())
        .collect()
}

pub(crate) fn package_source_dependency_entries(source: &PackageSource) -> BTreeSet<String> {
    relative_module_specifier_targets(source.source.as_str())
        .into_iter()
        .filter_map(|target| {
            resolve_package_relative_require(package_source_entry_path(source).as_str(), &target)
        })
        .map(|entry| {
            strip_source_extension(entry.as_str())
                .trim_matches('/')
                .to_ascii_lowercase()
        })
        .filter(|entry| !entry.is_empty())
        .collect()
}

fn relative_require_targets_package_source(
    external: &PackageSource,
    target: &str,
    matched: &PackageSource,
) -> bool {
    let Some(resolved) =
        resolve_package_relative_require(package_source_entry_path(external).as_str(), target)
    else {
        return false;
    };
    source_entry_paths_match(
        resolved.as_str(),
        package_source_entry_path(matched).as_str(),
    )
}

fn resolve_package_relative_require(from_entry: &str, target: &str) -> Option<String> {
    if !target.starts_with('.') {
        return None;
    }
    let from = from_entry.replace('\\', "/");
    let base = from
        .rsplit_once('/')
        .map(|(base, _file)| base)
        .unwrap_or_default();
    let joined = if base.is_empty() {
        target.to_string()
    } else {
        format!("{base}/{target}")
    };
    let mut segments = Vec::<&str>::new();
    for segment in joined.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }
    (!segments.is_empty()).then(|| segments.join("/"))
}

pub(crate) fn source_entry_paths_match(left: &str, right: &str) -> bool {
    let left = strip_source_extension(left)
        .trim_matches('/')
        .to_ascii_lowercase();
    let right = strip_source_extension(right)
        .trim_matches('/')
        .to_ascii_lowercase();
    left == right || format!("{left}/index") == right || left == format!("{right}/index")
}

pub(crate) fn package_source_entry_path_from_source_path(
    package_name: &str,
    package_version: &str,
    source_path: &str,
) -> String {
    let prefix = format!("{package_name}@{package_version}/");
    source_path
        .strip_prefix(prefix.as_str())
        .unwrap_or(source_path)
        .trim_start_matches('/')
        .to_ascii_lowercase()
}

fn package_source_cache_key(source: &PackageSource) -> String {
    format!(
        "{}@{}:{}",
        source.package_name, source.package_version, source.source_path
    )
}

fn dependency_graph_concrete_neighborhood_signature(
    dependency_ids: &[ModuleId],
    dependent_ids: &[ModuleId],
    candidate: &PackageSource,
    concrete_sources_by_module: &BTreeMap<ModuleId, ConcretePackageSourcePath>,
) -> String {
    let mut parts = Vec::new();
    for dependency_id in dependency_ids {
        if let Some(concrete) = concrete_sources_by_module.get(dependency_id)
            && concrete.package_name == candidate.package_name
            && concrete.package_version == candidate.package_version
        {
            parts.push(format!("d{}={}", dependency_id.0, concrete.source_path));
        }
    }
    for dependent_id in dependent_ids {
        if let Some(concrete) = concrete_sources_by_module.get(dependent_id)
            && concrete.package_name == candidate.package_name
            && concrete.package_version == candidate.package_version
        {
            parts.push(format!("r{}={}", dependent_id.0, concrete.source_path));
        }
    }
    parts.join("|")
}

fn export_member_proof_source_path(
    source: &PackageSource,
    proof: ExportMemberSourceProof,
    members: &BTreeSet<String>,
    aliases: &BTreeMap<String, String>,
) -> String {
    let members = members
        .iter()
        .take(64)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(",");
    let alias_proof = export_member_alias_proof_fragment(aliases);
    if !alias_proof.is_empty() {
        return format!(
            "forced-external:export-members:{}:{}:aliases={}:{}",
            export_member_source_proof_label(proof),
            members,
            alias_proof,
            source.source_path
        );
    }
    format!(
        "forced-external:export-members:{}:{}:{}",
        export_member_source_proof_label(proof),
        members,
        source.source_path
    )
}

fn export_member_alias_proof_fragment(aliases: &BTreeMap<String, String>) -> String {
    aliases
        .iter()
        .take(64)
        .filter(|(local, exported)| {
            local.as_str() != exported.as_str()
                && is_identifier_name(local.as_str())
                && is_identifier_name(exported.as_str())
        })
        .map(|(local, exported)| format!("{local}={exported}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn export_member_proof_fragment(members: &BTreeSet<String>) -> String {
    members
        .iter()
        .take(64)
        .filter(|member| is_identifier_name(member.as_str()))
        .cloned()
        .collect::<Vec<_>>()
        .join(",")
}

fn export_member_alias_proof_map(
    module_source: &str,
    external_source: &str,
    exported_members: &BTreeSet<String>,
) -> BTreeMap<String, String> {
    if module_source.trim().is_empty() || exported_members.is_empty() {
        return BTreeMap::new();
    }
    let local_signatures = binding_string_signatures_from_source(module_source);
    let external_signatures = binding_string_signatures_from_source(external_source)
        .into_iter()
        .filter(|(binding, signature)| {
            exported_members.contains(binding.as_str()) && !signature.is_empty()
        })
        .collect::<BTreeMap<_, _>>();
    if local_signatures.is_empty() || external_signatures.is_empty() {
        return BTreeMap::new();
    }

    let mut aliases = BTreeMap::new();
    for (local, local_signature) in local_signatures {
        if exported_members.contains(local.as_str()) || local_signature.is_empty() {
            continue;
        }
        let mut scored = external_signatures
            .iter()
            .filter_map(|(exported, external_signature)| {
                export_member_alias_score(&local_signature, exported.as_str(), external_signature)
                    .map(|score| (exported.as_str(), score))
            })
            .collect::<Vec<_>>();
        scored.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
        let Some((best_exported, best_score)) = scored.first().copied() else {
            continue;
        };
        if scored
            .get(1)
            .is_some_and(|(_exported, score)| *score == best_score)
        {
            continue;
        }
        aliases.insert(local, best_exported.to_string());
    }
    aliases
}

fn export_member_alias_score(
    local_signature: &BTreeSet<String>,
    exported_member: &str,
    external_signature: &BTreeSet<String>,
) -> Option<usize> {
    if local_signature.contains(exported_member) && external_signature.contains(exported_member) {
        return Some(10_000 + local_signature.intersection(external_signature).count());
    }
    let overlap = local_signature.intersection(external_signature).count();
    if overlap < 3 {
        return None;
    }
    let smaller = local_signature.len().min(external_signature.len());
    (overlap * 100 >= smaller * 80).then_some(1_000 + overlap)
}

fn unmatched_package_scope(rows: &InputRows) -> BTreeSet<String> {
    rows.modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
        .filter(|module| !has_accepted_external_attribution(rows, module.id))
        .filter_map(|module| {
            module
                .package_name
                .as_deref()
                .map(str::trim)
                .filter(|package_name| {
                    !package_name.is_empty() && is_valid_package_name(package_name)
                })
                .map(ToOwned::to_owned)
        })
        .collect()
}

pub(crate) fn concrete_package_sources_by_module(
    rows: &InputRows,
    report: &VersionedPackageMatchReport,
) -> BTreeMap<ModuleId, ConcretePackageSourcePath> {
    let mut sources = BTreeMap::new();
    for attribution in rows
        .package_attributions
        .iter()
        .chain(report.attributions.iter())
    {
        if attribution.status != PackageAttributionStatus::Accepted
            || attribution.emission_mode != PackageEmissionMode::ExternalImport
        {
            continue;
        }
        let Some(package_version) = attribution.package_version.as_deref() else {
            continue;
        };
        let Some(resolved_file) = attribution.resolved_file.as_deref() else {
            continue;
        };
        if let Some(concrete) = concrete_package_source_from_parts(
            attribution.module_id,
            attribution.package_name.as_str(),
            package_version,
            resolved_file,
        ) {
            sources.insert(attribution.module_id, concrete);
        }
    }
    for package_match in &report.matches {
        if let Some(concrete) = concrete_package_source_from_parts(
            package_match.module_id,
            package_match.package_name.as_str(),
            package_match.package_version.as_str(),
            package_match.source_path.as_str(),
        ) {
            sources.entry(package_match.module_id).or_insert(concrete);
        }
    }
    sources
}

pub(crate) fn concrete_package_source_from_parts(
    _module_id: ModuleId,
    package_name: &str,
    package_version: &str,
    proof_path: &str,
) -> Option<ConcretePackageSourcePath> {
    let source_path = concrete_package_source_path_from_proof(proof_path)?;
    Some(ConcretePackageSourcePath {
        package_name: package_name.to_string(),
        package_version: package_version.to_string(),
        source_path,
    })
}

fn concrete_package_source_path_from_proof(proof_path: &str) -> Option<String> {
    let proof_path = proof_path.trim();
    if proof_path.is_empty()
        || proof_path.starts_with("exact-hint:")
        || proof_path.starts_with("dependency-closure:")
        || proof_path.starts_with("dependency-cluster:")
        || proof_path.starts_with("package-file-graph:")
        || proof_path.starts_with("aggregate:")
        || proof_path.starts_with("cascade:")
        || proof_path.starts_with("structural-bag:")
    {
        return None;
    }
    if let Some(source_path) = external_import_concrete_source_path(proof_path) {
        return Some(source_path);
    }
    Some(proof_path.to_string())
}

pub(crate) fn package_version_from_proof_path(
    package_name: &str,
    proof_path: &str,
) -> Option<String> {
    let concrete = concrete_package_source_path_from_proof(proof_path)?;
    let prefix = format!("{package_name}@");
    let rest = concrete.strip_prefix(prefix.as_str())?;
    let (version, _path) = rest.split_once('/')?;
    (!version.trim().is_empty()).then(|| version.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DependencyGraphSourceProof {
    ExactSourceHash,
    FunctionStringFingerprint,
    DependencyNeighborhood,
    StringFingerprintWithGraph,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DependencyGraphEvidence {
    matched_edges: usize,
    known_edges: usize,
}

#[derive(Debug, Clone, Copy)]
struct DependencyGraphSourceCandidate<'a> {
    source: &'a PackageSource,
    proof: DependencyGraphSourceProof,
    graph: DependencyGraphEvidence,
    function_matches: usize,
    string_matches: usize,
}

pub(crate) fn dependency_graph_source_fingerprint_external_import_target<'a>(
    rows: &InputRows,
    module: &ModuleInput,
    package_match: &PackageMatch,
    external_source_index: &ExternalImportSourceIndex<'a>,
    module_source: &str,
    concrete_sources_by_module: &BTreeMap<ModuleId, ConcretePackageSourcePath>,
    cache: &ExternalImportProofScratch<'a>,
) -> Option<ExternalImportTarget> {
    if !dependency_graph_source_fingerprint_policy_allows(package_match.strategy)
        || module_source.trim().is_empty()
    {
        return None;
    }
    let module_fingerprint =
        cache.module_fingerprint(module, module.semantic_path.as_str(), module_source)?;
    let mut candidates = cache
        .source_fingerprints_for_version(
            external_source_index,
            package_match.package_name.as_str(),
            package_match.package_version.as_str(),
        )
        .into_iter()
        .filter_map(|source_fingerprint| {
            let source = source_fingerprint.source;
            let graph = cache.dependency_graph_evidence(
                rows,
                module.id,
                source,
                external_source_index,
                concrete_sources_by_module,
            );
            let function_matches = source_fingerprint
                .function_signature_hashes
                .intersection(&module_fingerprint.function_signature_hashes)
                .count();
            let string_matches = source_fingerprint
                .string_anchors
                .intersection(&module_fingerprint.string_anchors)
                .count();
            let proof = dependency_graph_source_proof(
                &module_fingerprint,
                &source_fingerprint,
                graph,
                function_matches,
                string_matches,
            )?;
            Some(DependencyGraphSourceCandidate {
                source,
                proof,
                graph,
                function_matches,
                string_matches,
            })
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by(|left, right| {
        dependency_graph_source_candidate_score(right)
            .cmp(&dependency_graph_source_candidate_score(left))
            .then_with(|| {
                package_source_external_import_rank(left.source)
                    .cmp(&package_source_external_import_rank(right.source))
            })
            .then_with(|| {
                left.source
                    .export_specifier
                    .cmp(&right.source.export_specifier)
            })
            .then_with(|| left.source.source_path.cmp(&right.source.source_path))
    });
    let best_score = dependency_graph_source_candidate_score(candidates.first()?);
    let best = candidates
        .into_iter()
        .filter(|candidate| dependency_graph_source_candidate_score(candidate) == best_score)
        .collect::<Vec<_>>();
    let best_proof = best.first()?.proof;
    let export_specifiers = best
        .iter()
        .map(|candidate| candidate.source.export_specifier.as_str())
        .collect::<BTreeSet<_>>();
    if export_specifiers.len() != 1 {
        return None;
    }
    if dependency_graph_source_proof_requires_unique_source_path(best_proof) {
        let targets = best
            .iter()
            .map(|candidate| {
                (
                    candidate.source.export_specifier.as_str(),
                    candidate.source.source_path.as_str(),
                )
            })
            .collect::<BTreeSet<_>>();
        if targets.len() != 1 {
            return None;
        }
    }
    let selected = best.into_iter().min_by(|left, right| {
        package_source_external_import_rank(left.source)
            .cmp(&package_source_external_import_rank(right.source))
            .then_with(|| left.source.source_path.cmp(&right.source.source_path))
    })?;
    if selected.source.external_importable {
        return Some(ExternalImportTarget {
            export_specifier: selected.source.export_specifier.clone(),
            source_path: format!(
                "forced-external:dependency-graph-source:{}:graph={}/{}:functions={}:strings={}:{}",
                dependency_graph_source_proof_label(selected.proof),
                selected.graph.matched_edges,
                selected.graph.known_edges,
                selected.function_matches,
                selected.string_matches,
                selected.source.source_path,
            ),
        });
    }
    export_member_external_package_source_for_source_path(
        selected.source.package_name.as_str(),
        selected.source.package_version.as_str(),
        selected.source.source_path.as_str(),
        external_source_index,
        module_source,
    )
}

fn dependency_graph_source_candidate_score(
    candidate: &DependencyGraphSourceCandidate<'_>,
) -> usize {
    dependency_graph_source_proof_rank(candidate.proof)
        + candidate.graph.matched_edges * 20
        + candidate.function_matches * 3
        + candidate.string_matches
}

fn dependency_graph_source_proof(
    module_fingerprint: &ModuleMatchFingerprint,
    source_fingerprint: &PackageSourceFingerprint<'_>,
    graph: DependencyGraphEvidence,
    function_matches: usize,
    string_matches: usize,
) -> Option<DependencyGraphSourceProof> {
    if !source_fingerprint
        .normalized_source_hashes
        .is_disjoint(&module_fingerprint.normalized_source_hashes)
    {
        return Some(DependencyGraphSourceProof::ExactSourceHash);
    }
    if graph.matched_edges >= 1 && function_matches >= 2 && string_matches >= 1 {
        return Some(DependencyGraphSourceProof::FunctionStringFingerprint);
    }
    if graph.matched_edges >= 1 && string_matches >= 8 {
        return Some(DependencyGraphSourceProof::StringFingerprintWithGraph);
    }
    if graph.matched_edges >= 2 && string_matches >= 3 {
        return Some(DependencyGraphSourceProof::StringFingerprintWithGraph);
    }
    if graph.known_edges >= 2 && graph.matched_edges == graph.known_edges {
        return Some(DependencyGraphSourceProof::DependencyNeighborhood);
    }
    None
}

fn dependency_graph_source_evidence(
    candidate: &PackageSource,
    external_source_index: &ExternalImportSourceIndex<'_>,
    concrete_sources_by_module: &BTreeMap<ModuleId, ConcretePackageSourcePath>,
    dependency_ids: &[ModuleId],
    dependent_ids: &[ModuleId],
) -> DependencyGraphEvidence {
    let candidate_entry = package_source_entry_path(candidate);
    let candidate_deps = external_source_index.dependency_entries(candidate);
    let mut known_edges = 0usize;
    let mut matched_edges = 0usize;

    for dependency_id in dependency_ids {
        let Some(neighbor) = concrete_sources_by_module.get(dependency_id) else {
            continue;
        };
        if neighbor.package_name != candidate.package_name
            || neighbor.package_version != candidate.package_version
        {
            continue;
        }
        known_edges += 1;
        let neighbor_entry = package_source_entry_path_from_source_path(
            neighbor.package_name.as_str(),
            neighbor.package_version.as_str(),
            neighbor.source_path.as_str(),
        );
        if candidate_deps
            .iter()
            .any(|target| source_entry_paths_match(target.as_str(), neighbor_entry.as_str()))
        {
            matched_edges += 1;
        }
    }

    for dependent_id in dependent_ids {
        let Some(neighbor) = concrete_sources_by_module.get(dependent_id) else {
            continue;
        };
        if neighbor.package_name != candidate.package_name
            || neighbor.package_version != candidate.package_version
        {
            continue;
        }
        let neighbor_sources = external_source_index.sources_matching_concrete_path(
            neighbor.package_name.as_str(),
            neighbor.package_version.as_str(),
            neighbor.source_path.as_str(),
        );
        if neighbor_sources.is_empty() {
            continue;
        }
        known_edges += 1;
        if neighbor_sources.iter().any(|neighbor_source| {
            external_source_index
                .dependency_entries(neighbor_source)
                .iter()
                .any(|target| source_entry_paths_match(target.as_str(), candidate_entry.as_str()))
        }) {
            matched_edges += 1;
        }
    }

    DependencyGraphEvidence {
        matched_edges,
        known_edges,
    }
}

pub(crate) fn dependency_edge_path_external_import_target(
    rows: &InputRows,
    module: &ModuleInput,
    package_match: &PackageMatch,
    external_source_index: &ExternalImportSourceIndex<'_>,
    concrete_sources_by_module: &BTreeMap<ModuleId, ConcretePackageSourcePath>,
    cache: &ExternalImportProofScratch<'_>,
) -> Option<ExternalImportTarget> {
    if !dependency_edge_path_policy_allows(package_match) {
        return None;
    }
    let mut candidates = Vec::<DependencyEdgePathCandidate<'_>>::new();
    for dependent_id in cache.direct_dependents(rows, module.id) {
        let Some(dependent) = concrete_sources_by_module.get(&dependent_id) else {
            continue;
        };
        if dependent.package_name != package_match.package_name
            || dependent.package_version != package_match.package_version
        {
            continue;
        }
        let dependent_sources = external_source_index.sources_matching_concrete_path(
            dependent.package_name.as_str(),
            dependent.package_version.as_str(),
            dependent.source_path.as_str(),
        );
        for dependent_source in dependent_sources {
            let entries = dependency_edge_path_remaining_entries(
                rows,
                dependent_id,
                module.id,
                dependent_source,
                external_source_index,
                concrete_sources_by_module,
                cache,
            );
            if entries.len() != 1 {
                continue;
            }
            let entry = entries
                .into_iter()
                .next()
                .expect("one remaining dependency entry");
            for source in external_importable_sources_matching_entry(
                package_match.package_name.as_str(),
                package_match.package_version.as_str(),
                entry.as_str(),
                external_source_index,
            ) {
                candidates.push(DependencyEdgePathCandidate {
                    source,
                    dependent_id,
                    dependent_source_path: dependent_source.source_path.as_str(),
                    entry: entry.clone(),
                });
            }
        }
    }
    if candidates.is_empty() {
        return None;
    }
    let targets = candidates
        .iter()
        .map(|candidate| {
            (
                candidate.source.export_specifier.as_str(),
                candidate.source.source_path.as_str(),
            )
        })
        .collect::<BTreeSet<_>>();
    if targets.len() != 1 {
        return None;
    }
    let selected = candidates.into_iter().min_by(|left, right| {
        package_source_external_import_rank(left.source)
            .cmp(&package_source_external_import_rank(right.source))
            .then_with(|| left.source.source_path.cmp(&right.source.source_path))
            .then_with(|| left.dependent_id.cmp(&right.dependent_id))
    })?;
    Some(ExternalImportTarget {
        export_specifier: selected.source.export_specifier.clone(),
        source_path: format!(
            "forced-external:dependency-edge-path:dependent={}:entry={}:from={}:{}",
            selected.dependent_id.0,
            selected.entry,
            selected.dependent_source_path,
            selected.source.source_path,
        ),
    })
}

#[derive(Debug, Clone)]
struct DependencyEdgePathCandidate<'a> {
    source: &'a PackageSource,
    dependent_id: ModuleId,
    dependent_source_path: &'a str,
    entry: String,
}

fn dependency_edge_path_remaining_entries(
    rows: &InputRows,
    dependent_id: ModuleId,
    unresolved_module_id: ModuleId,
    dependent_source: &PackageSource,
    external_source_index: &ExternalImportSourceIndex<'_>,
    concrete_sources_by_module: &BTreeMap<ModuleId, ConcretePackageSourcePath>,
    cache: &ExternalImportProofScratch<'_>,
) -> BTreeSet<String> {
    let dependency_ids = cache.direct_dependencies(rows, dependent_id);
    if !dependency_ids.contains(&unresolved_module_id) {
        return BTreeSet::new();
    }
    let mut entries = external_source_index.dependency_entries(dependent_source);
    if entries.is_empty() {
        return entries;
    }
    for dependency_id in dependency_ids {
        if dependency_id == unresolved_module_id {
            continue;
        }
        if let Some(concrete) = concrete_sources_by_module.get(&dependency_id) {
            if concrete.package_name == dependent_source.package_name
                && concrete.package_version == dependent_source.package_version
            {
                let known_entry = package_source_entry_path_from_source_path(
                    concrete.package_name.as_str(),
                    concrete.package_version.as_str(),
                    concrete.source_path.as_str(),
                );
                entries.retain(|entry| {
                    !source_entry_paths_match(entry.as_str(), known_entry.as_str())
                });
            }
            continue;
        }
        if cache.row_module_is_same_package_version(
            rows,
            dependency_id,
            dependent_source.package_name.as_str(),
            dependent_source.package_version.as_str(),
        ) {
            return BTreeSet::new();
        }
    }
    entries
}

fn external_importable_sources_matching_entry<'a>(
    package_name: &str,
    package_version: &str,
    entry: &str,
    external_source_index: &'a ExternalImportSourceIndex<'a>,
) -> Vec<&'a PackageSource> {
    sources_matching_entry(package_name, package_version, entry, external_source_index)
        .into_iter()
        .filter(|source| source.external_importable)
        .collect()
}

fn sources_matching_entry<'a>(
    package_name: &str,
    package_version: &str,
    entry: &str,
    external_source_index: &'a ExternalImportSourceIndex<'a>,
) -> Vec<&'a PackageSource> {
    external_source_index
        .all_sources(package_name, package_version)
        .iter()
        .copied()
        .filter(|source| {
            source_entry_paths_match(package_source_entry_path(source).as_str(), entry)
        })
        .collect()
}

#[derive(Debug, Clone)]
struct CrossVersionSourceCandidate {
    package_match: ModulePackageMatch,
    target: ExternalImportTarget,
}

pub(crate) fn same_package_cross_version_source_external_import_target<'a>(
    module: &ModuleInput,
    package_match: &PackageMatch,
    external_source_index: &ExternalImportSourceIndex<'a>,
    module_source: &str,
    cache: &ExternalImportProofScratch<'a>,
) -> Option<CorrectedPackageExternalImportTarget> {
    if !same_package_cross_version_source_policy_allows(package_match)
        || module_source.trim().is_empty()
    {
        return None;
    }
    let module_fingerprint =
        cache.module_fingerprint(module, module.semantic_path.as_str(), module_source)?;
    let mut by_version = BTreeMap::<String, Vec<PackageSourceFingerprint<'_>>>::new();
    for source_fingerprint in cache
        .source_fingerprints_for_package(external_source_index, package_match.package_name.as_str())
    {
        if source_fingerprint.source.package_version == package_match.package_version {
            continue;
        }
        by_version
            .entry(source_fingerprint.source.package_version.clone())
            .or_default()
            .push(source_fingerprint);
    }
    let mut candidates = Vec::<CrossVersionSourceCandidate>::new();
    for (package_version, sources) in by_version {
        let version = PackageVersionCandidate {
            package_name: package_match.package_name.clone(),
            package_version,
            sources,
        };
        let Some(source_match) = best_source_match(
            &version,
            &module_fingerprint,
            &VersionedPackageMatcherConfig::default(),
        ) else {
            continue;
        };
        if !source_only_match_can_be_promoted_to_import(source_match.strategy) {
            continue;
        }
        let target = if source_match.external_importable {
            ExternalImportTarget {
                export_specifier: source_match.export_specifier.clone(),
                source_path: format!(
                    "forced-external:cross-version-source:{}:from={}:{}",
                    source_match.strategy.as_str(),
                    package_match.package_version,
                    source_match.source_path
                ),
            }
        } else {
            export_member_external_package_source_for_source_path(
                source_match.package_name.as_str(),
                source_match.package_version.as_str(),
                source_match.source_path.as_str(),
                external_source_index,
                module_source,
            )?
        };
        if !cross_version_source_target_allowed_by_runtime_surface(
            package_match,
            &source_match,
            &target,
            external_source_index,
        ) {
            continue;
        }
        candidates.push(CrossVersionSourceCandidate {
            package_match: source_match,
            target,
        });
    }
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by(|left, right| {
        cross_version_source_candidate_score(&right.package_match)
            .cmp(&cross_version_source_candidate_score(&left.package_match))
            .then_with(|| {
                left.package_match
                    .package_version
                    .cmp(&right.package_match.package_version)
            })
            .then_with(|| {
                left.package_match
                    .export_specifier
                    .cmp(&right.package_match.export_specifier)
            })
            .then_with(|| {
                left.package_match
                    .source_path
                    .cmp(&right.package_match.source_path)
            })
    });
    let best_score = cross_version_source_candidate_score(&candidates.first()?.package_match);
    let best = candidates
        .into_iter()
        .filter(|candidate| {
            cross_version_source_candidate_score(&candidate.package_match) == best_score
        })
        .collect::<Vec<_>>();
    let targets = best
        .iter()
        .map(|candidate| {
            (
                candidate.package_match.package_name.as_str(),
                candidate.package_match.package_version.as_str(),
                candidate.target.export_specifier.as_str(),
                candidate.target.source_path.as_str(),
            )
        })
        .collect::<BTreeSet<_>>();
    if targets.len() != 1 {
        return None;
    }
    let selected = best.into_iter().next()?;
    Some(CorrectedPackageExternalImportTarget {
        package_name: selected.package_match.package_name,
        package_version: selected.package_match.package_version,
        target: selected.target,
        function_signature_matches: selected.package_match.function_signature_matches,
        string_anchor_matches: selected.package_match.string_anchor_matches,
    })
}

fn cross_version_source_target_allowed_by_runtime_surface(
    package_match: &PackageMatch,
    source_match: &ModulePackageMatch,
    target: &ExternalImportTarget,
    external_source_index: &ExternalImportSourceIndex<'_>,
) -> bool {
    if !cross_version_source_proof_is_older_than_hint(
        source_match.package_version.as_str(),
        package_match.package_version.as_str(),
    ) {
        return true;
    }
    external_source_index
        .sources(
            package_match.package_name.as_str(),
            package_match.package_version.as_str(),
        )
        .iter()
        .any(|source| source.export_specifier == target.export_specifier)
}

fn cross_version_source_proof_is_older_than_hint(proof_version: &str, hint_version: &str) -> bool {
    match (Version::parse(proof_version), Version::parse(hint_version)) {
        (Ok(proof_version), Ok(hint_version)) => proof_version < hint_version,
        _ => proof_version != hint_version,
    }
}

fn cross_version_source_candidate_score(package_match: &ModulePackageMatch) -> usize {
    let strategy_score = match package_match.strategy {
        ModuleMatchStrategy::NormalizedSourceHash => 1000,
        ModuleMatchStrategy::FunctionSignatureAndStringAnchors => 700,
        ModuleMatchStrategy::PropertyShapeAndStringAnchors
        | ModuleMatchStrategy::ObjectShapeAndStringAnchors
        | ModuleMatchStrategy::ClassShapeAndStringAnchors
        | ModuleMatchStrategy::SwitchShapeAndStringAnchors => 600,
        ModuleMatchStrategy::AggregateFunctionSignatureAndStringAnchors
        | ModuleMatchStrategy::CascadeFunctionCoverage
        | ModuleMatchStrategy::CascadeFunctionOwnership
        | ModuleMatchStrategy::CascadePartialFunctionCoverage
        | ModuleMatchStrategy::AggregateStructuralBagSimilarity
        | ModuleMatchStrategy::DependencyClosureOwnership => 0,
    };
    strategy_score
        + package_match.function_signature_matches * 3
        + package_match.string_anchor_matches
}

pub(crate) fn cross_package_exact_source_external_import_target<'a>(
    rows: &InputRows,
    module: &ModuleInput,
    package_match: &PackageMatch,
    external_source_index: &ExternalImportSourceIndex<'a>,
    module_source: &str,
    concrete_sources_by_module: &BTreeMap<ModuleId, ConcretePackageSourcePath>,
    cache: &ExternalImportProofScratch<'a>,
) -> Option<CorrectedPackageExternalImportTarget> {
    if !cross_package_exact_source_policy_allows(package_match) || module_source.trim().is_empty() {
        return None;
    }
    let module_fingerprint =
        cache.module_fingerprint(module, module.semantic_path.as_str(), module_source)?;
    let mut candidates = external_source_index
        .normalized_sources_for_any_package(&module_fingerprint.normalized_source_hashes)
        .into_iter()
        .filter(|source| source.external_importable && source.is_within_fingerprint_budget())
        .filter_map(|source| {
            let source_fingerprint = external_source_index.source_fingerprint(source)?;
            if source_fingerprint
                .normalized_source_hashes
                .is_disjoint(&module_fingerprint.normalized_source_hashes)
            {
                return None;
            }
            let function_matches = source_fingerprint
                .function_signature_hashes
                .intersection(&module_fingerprint.function_signature_hashes)
                .count();
            let string_matches = source_fingerprint
                .string_anchors
                .intersection(&module_fingerprint.string_anchors)
                .count();
            let graph = cache.dependency_graph_evidence(
                rows,
                module.id,
                source,
                external_source_index,
                concrete_sources_by_module,
            );
            if !cross_package_exact_source_candidate_allowed(
                module_source,
                source,
                graph,
                function_matches,
                string_matches,
            ) {
                return None;
            }
            Some(CrossPackageExactSourceCandidate {
                source,
                graph,
                function_matches,
                string_matches,
            })
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by(|left, right| {
        cross_package_exact_source_score(right)
            .cmp(&cross_package_exact_source_score(left))
            .then_with(|| {
                package_source_external_import_rank(left.source)
                    .cmp(&package_source_external_import_rank(right.source))
            })
            .then_with(|| left.source.package_name.cmp(&right.source.package_name))
            .then_with(|| {
                left.source
                    .package_version
                    .cmp(&right.source.package_version)
            })
            .then_with(|| {
                left.source
                    .export_specifier
                    .cmp(&right.source.export_specifier)
            })
            .then_with(|| left.source.source_path.cmp(&right.source.source_path))
    });
    let best_score = cross_package_exact_source_score(candidates.first()?);
    let best = candidates
        .into_iter()
        .filter(|candidate| cross_package_exact_source_score(candidate) == best_score)
        .collect::<Vec<_>>();
    let targets = best
        .iter()
        .map(|candidate| {
            (
                candidate.source.package_name.as_str(),
                candidate.source.package_version.as_str(),
                candidate.source.export_specifier.as_str(),
                candidate.source.source_path.as_str(),
            )
        })
        .collect::<BTreeSet<_>>();
    if targets.len() != 1 {
        return None;
    }
    let selected = best.into_iter().next()?;
    Some(CorrectedPackageExternalImportTarget {
        package_name: selected.source.package_name.clone(),
        package_version: selected.source.package_version.clone(),
        function_signature_matches: selected.function_matches,
        string_anchor_matches: selected.string_matches,
        target: ExternalImportTarget {
            export_specifier: selected.source.export_specifier.clone(),
            source_path: format!(
                "forced-external:cross-package-source:source-hash:hint={}@{}:graph={}/{}:functions={}:strings={}:{}",
                package_match.package_name,
                package_match.package_version,
                selected.graph.matched_edges,
                selected.graph.known_edges,
                selected.function_matches,
                selected.string_matches,
                selected.source.source_path,
            ),
        },
    })
}

#[derive(Debug, Clone, Copy)]
struct CrossPackageExactSourceCandidate<'a> {
    source: &'a PackageSource,
    graph: DependencyGraphEvidence,
    function_matches: usize,
    string_matches: usize,
}

fn cross_package_exact_source_candidate_allowed(
    module_source: &str,
    source: &PackageSource,
    graph: DependencyGraphEvidence,
    function_matches: usize,
    string_matches: usize,
) -> bool {
    if is_json_source_path(source.source_path.as_str()) {
        return false;
    }
    graph.matched_edges >= 1
        || (module_source.len() >= 120 && function_matches >= 1 && string_matches >= 1)
        || (module_source.len() >= 300 && (function_matches >= 1 || string_matches >= 2))
}

fn cross_package_exact_source_score(candidate: &CrossPackageExactSourceCandidate<'_>) -> usize {
    1_000
        + candidate.graph.matched_edges * 50
        + candidate.function_matches * 10
        + candidate.string_matches
}

pub(crate) fn forced_external_package_version(
    module: &ModuleInput,
    source_only_match: Option<&PackageMatch>,
    package_sources: &[PackageSource],
) -> Option<String> {
    module
        .package_version
        .as_deref()
        .map(str::trim)
        .filter(|version| !version.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| source_only_match.map(|package_match| package_match.package_version.clone()))
        .or_else(|| {
            latest_package_source_version(package_sources, module.package_name.as_deref()?.trim())
        })
}

fn latest_package_source_version(
    package_sources: &[PackageSource],
    package_name: &str,
) -> Option<String> {
    package_sources
        .iter()
        .filter(|source| source.package_name == package_name)
        .filter_map(|source| {
            Version::parse(source.package_version.as_str())
                .ok()
                .map(|version| (version, source.package_version.as_str()))
        })
        .max_by(|left, right| left.0.cmp(&right.0))
        .map(|(_version, text)| text.to_string())
}

pub(crate) fn forced_external_import_target(
    rows: &InputRows,
    module: &ModuleInput,
    package_name: &str,
    package_version: &str,
    source_only_match: Option<&PackageMatch>,
    external_source_index: &ExternalImportSourceIndex<'_>,
) -> Option<ExternalImportTarget> {
    let module_source = rows
        .module_source_slice(module.id)
        .map(|slice| slice.source)
        .unwrap_or_default();
    resolve_external_import_target_with_index(
        module,
        package_name,
        package_version,
        source_only_match,
        external_source_index,
        module_source,
    )
}

fn has_accepted_attribution(rows: &InputRows, module_id: ModuleId) -> bool {
    rows.package_attributions.iter().any(|attribution| {
        attribution.module_id == module_id
            && attribution.status == PackageAttributionStatus::Accepted
            && attribution.emission_mode == PackageEmissionMode::ExternalImport
    })
}

pub(crate) fn has_accepted_surface(rows: &InputRows, specifier: &str) -> bool {
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

const CASCADE_MATCHED_MODULE_SOURCE_LIMIT: usize = 8;
const CASCADE_PIPELINE_SOURCE_LIMIT: usize = 4096;
const CASCADE_SOURCE_GROUP_LIMIT: usize = 128;

#[cfg(test)]
mod tests;
