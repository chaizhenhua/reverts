use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use reverts_graph::FunctionExtractor;
use reverts_input::{InputRows, PackageAttributionInput};
use reverts_ir::{FunctionFingerprint, ModuleId, ModuleKind};
use reverts_observe::AuditReport;

use crate::index::package_module_source_quality;
use crate::model::{
    PackageMatchingPipelineReport, PackageModuleSourceQuality, PackageSource,
    VersionedPackageMatchReport,
};
use crate::proof::concrete_source::unmatched_package_scope;
use crate::source::cache_surfaces::append_cache_anchored_package_surfaces;
use crate::strategy::{
    self, CascadeMatchReport, match_structural_bags_with_excluded_modules, match_with_cascade,
};
use crate::{VersionedPackageMatcher, ownership};

const CASCADE_MATCHED_MODULE_SOURCE_LIMIT: usize = 8;
const CASCADE_PIPELINE_SOURCE_LIMIT: usize = 4096;
const CASCADE_SOURCE_GROUP_LIMIT: usize = 128;

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
    let context = PackageMatchContext {
        rows,
        package_sources,
        package_filter,
    };
    let mut state = PackageMatchState::new();
    let mut timing = PipelineTiming::from_env();

    run_package_match_pass(VersionedMatcherPass, &context, &mut state, &mut timing);
    run_package_match_pass(
        ModuleFunctionFingerprintsPass,
        &context,
        &mut state,
        &mut timing,
    );
    run_package_match_pass(CascadeMatchPass, &context, &mut state, &mut timing);
    run_package_match_pass(StructuralBagPass, &context, &mut state, &mut timing);
    run_package_match_pass(WeakSourceEquivalentPass, &context, &mut state, &mut timing);
    run_package_match_pass(ExactHintPass, &context, &mut state, &mut timing);
    run_package_match_pass(DependencyClosurePass, &context, &mut state, &mut timing);
    run_package_match_pass(DependencyClusterPass, &context, &mut state, &mut timing);
    run_package_match_pass(PackageFileGraphPass, &context, &mut state, &mut timing);
    run_package_match_pass(ImportablePass, &context, &mut state, &mut timing);
    run_package_match_pass(ForceExternalizePass, &context, &mut state, &mut timing);
    run_package_match_pass(CacheAnchoredSurfacesPass, &context, &mut state, &mut timing);

    PackageMatchingPipelineReport {
        package_report: state.package_report,
        function_attributions: state.function_attributions,
        function_ownership_matches: state.function_ownership_matches,
    }
}

/// Immutable inputs shared by every package-matching pass.
///
/// Keeping the context explicit prevents individual passes from reaching out to
/// unrelated global state and makes package-name filtering and source limits
/// visible at the orchestration layer.
struct PackageMatchContext<'a> {
    rows: &'a InputRows,
    package_sources: &'a [PackageSource],
    package_filter: Option<&'a BTreeSet<String>>,
}

impl PackageMatchContext<'_> {
    fn cascade_disabled(&self) -> bool {
        self.package_sources.len() > CASCADE_PIPELINE_SOURCE_LIMIT
    }

    fn restrict_function_inputs_to_weak_sources(&self) -> bool {
        self.package_sources.len() > CASCADE_MATCHED_MODULE_SOURCE_LIMIT
    }
}

/// Mutable product of the package-matching pipeline.
///
/// Passes communicate only through this state object: concrete package report,
/// function-level evidence, and reusable per-module function fingerprints.
struct PackageMatchState {
    package_report: VersionedPackageMatchReport,
    fingerprints_by_module: BTreeMap<ModuleId, Vec<FunctionFingerprint>>,
    function_attributions: Vec<PackageAttributionInput>,
    function_ownership_matches: usize,
}

impl PackageMatchState {
    fn new() -> Self {
        Self {
            package_report: empty_versioned_package_match_report(),
            fingerprints_by_module: BTreeMap::new(),
            function_attributions: Vec::new(),
            function_ownership_matches: 0,
        }
    }
}

trait PackageMatchPass {
    fn name(&self) -> &'static str;

    fn run(&self, context: &PackageMatchContext<'_>, state: &mut PackageMatchState);
}

fn run_package_match_pass(
    pass: impl PackageMatchPass,
    context: &PackageMatchContext<'_>,
    state: &mut PackageMatchState,
    timing: &mut PipelineTiming,
) {
    let name = pass.name();
    pass.run(context, state);
    timing.mark(name);
}

struct PipelineTiming {
    enabled: bool,
    started: Instant,
    last: Instant,
}

impl PipelineTiming {
    fn from_env() -> Self {
        let now = Instant::now();
        Self {
            enabled: std::env::var_os("REVERTS_MATCH_TIMING").is_some(),
            started: now,
            last: now,
        }
    }

    fn mark(&mut self, stage: &str) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        eprintln!(
            "package-pipeline timing: {} stage={:.3}s total={:.3}s",
            stage,
            now.duration_since(self.last).as_secs_f64(),
            now.duration_since(self.started).as_secs_f64()
        );
        self.last = now;
    }
}

struct VersionedMatcherPass;

impl PackageMatchPass for VersionedMatcherPass {
    fn name(&self) -> &'static str {
        "versioned_matcher"
    }

    fn run(&self, context: &PackageMatchContext<'_>, state: &mut PackageMatchState) {
        state.package_report = if let Some(package_filter) = context.package_filter {
            VersionedPackageMatcher::default().match_rows_for_packages(
                context.rows,
                context.package_sources,
                package_filter,
            )
        } else {
            VersionedPackageMatcher::default().match_rows(context.rows, context.package_sources)
        };
    }
}

struct ModuleFunctionFingerprintsPass;

impl PackageMatchPass for ModuleFunctionFingerprintsPass {
    fn name(&self) -> &'static str {
        "module_function_fingerprints"
    }

    fn run(&self, context: &PackageMatchContext<'_>, state: &mut PackageMatchState) {
        if context.cascade_disabled() {
            state.fingerprints_by_module.clear();
            return;
        }
        let package_matched_modules = if context.restrict_function_inputs_to_weak_sources() {
            state
                .package_report
                .matches
                .iter()
                .map(|package_match| package_match.module_id)
                .collect::<BTreeSet<_>>()
        } else {
            BTreeSet::new()
        };
        state.fingerprints_by_module = fingerprints_from_rows(
            context.rows,
            context.package_filter,
            &package_matched_modules,
            context.restrict_function_inputs_to_weak_sources(),
        );
    }
}

struct CascadeMatchPass;

impl PackageMatchPass for CascadeMatchPass {
    fn name(&self) -> &'static str {
        "cascade_match"
    }

    fn run(&self, context: &PackageMatchContext<'_>, state: &mut PackageMatchState) {
        if context.cascade_disabled() {
            return;
        }
        let cascade_report = match_with_cascade_scoped_by_module_hints(
            context.rows,
            &state.fingerprints_by_module,
            context.package_sources,
        );
        ownership::cascade::promote_cascade_function_coverage_to_module_attributions(
            context.rows,
            &state.fingerprints_by_module,
            &cascade_report,
            &mut state.package_report,
        );
        state.function_attributions = cascade_report.attributions;
        state.function_ownership_matches = cascade_report.ownership_matches.len();
        state.package_report.audit.extend(cascade_report.audit);
    }
}

struct StructuralBagPass;

impl PackageMatchPass for StructuralBagPass {
    fn name(&self) -> &'static str {
        "structural_bag"
    }

    fn run(&self, context: &PackageMatchContext<'_>, state: &mut PackageMatchState) {
        if context.cascade_disabled() {
            return;
        }
        let excluded_modules = state
            .package_report
            .matches
            .iter()
            .map(|package_match| package_match.module_id)
            .collect::<BTreeSet<_>>();
        let structural_bag_report = match_structural_bags_with_excluded_modules(
            context.rows,
            context.package_sources,
            context.package_filter,
            &excluded_modules,
        );
        strategy::structural_bag::promote_structural_bag_ownership_matches(
            context.rows,
            structural_bag_report.matches.as_slice(),
            &mut state.package_report,
        );
        state
            .package_report
            .audit
            .extend(structural_bag_report.audit);
    }
}

struct WeakSourceEquivalentPass;

impl PackageMatchPass for WeakSourceEquivalentPass {
    fn name(&self) -> &'static str {
        "weak_source_equivalent"
    }

    fn run(&self, context: &PackageMatchContext<'_>, state: &mut PackageMatchState) {
        ownership::weak_source_equivalent::promote_weak_source_equivalent_matches(
            context.rows,
            context.package_sources,
            &mut state.package_report,
        );
    }
}

struct ExactHintPass;

impl PackageMatchPass for ExactHintPass {
    fn name(&self) -> &'static str {
        "exact_hint_promote"
    }

    fn run(&self, context: &PackageMatchContext<'_>, state: &mut PackageMatchState) {
        ownership::exact_hint::promote_exact_hint_ownership_matches(
            context.rows,
            context.package_sources,
            &mut state.package_report,
        );
    }
}

struct DependencyClosurePass;

impl PackageMatchPass for DependencyClosurePass {
    fn name(&self) -> &'static str {
        "dependency_closure"
    }

    fn run(&self, context: &PackageMatchContext<'_>, state: &mut PackageMatchState) {
        ownership::dependency_neighborhood::promote_dependency_closure_ownership_matches(
            context.rows,
            &mut state.package_report,
        );
    }
}

struct DependencyClusterPass;

impl PackageMatchPass for DependencyClusterPass {
    fn name(&self) -> &'static str {
        "dependency_cluster"
    }

    fn run(&self, context: &PackageMatchContext<'_>, state: &mut PackageMatchState) {
        ownership::dependency_neighborhood::promote_dependency_cluster_ownership_matches(
            context.rows,
            &mut state.package_report,
        );
    }
}

struct PackageFileGraphPass;

impl PackageMatchPass for PackageFileGraphPass {
    fn name(&self) -> &'static str {
        "package_file_graph"
    }

    fn run(&self, context: &PackageMatchContext<'_>, state: &mut PackageMatchState) {
        ownership::package_file_graph::promote_package_file_graph_ownership_matches(
            context.rows,
            &mut state.package_report,
        );
    }
}

struct ImportablePass;

impl PackageMatchPass for ImportablePass {
    fn name(&self) -> &'static str {
        "importable_promote"
    }

    fn run(&self, context: &PackageMatchContext<'_>, state: &mut PackageMatchState) {
        ownership::importable::promote_importable_ownership_matches(
            context.rows,
            context.package_sources,
            &mut state.package_report,
        );
    }
}

struct ForceExternalizePass;

impl PackageMatchPass for ForceExternalizePass {
    fn name(&self) -> &'static str {
        "force_externalize"
    }

    fn run(&self, context: &PackageMatchContext<'_>, state: &mut PackageMatchState) {
        let matched_package_names = context
            .package_filter
            .cloned()
            .unwrap_or_else(|| unmatched_package_scope(context.rows));
        ownership::force_externalize::force_externalize_remaining_package_modules(
            context.rows,
            context.package_sources,
            &matched_package_names,
            &mut state.package_report,
        );
    }
}

struct CacheAnchoredSurfacesPass;

impl PackageMatchPass for CacheAnchoredSurfacesPass {
    fn name(&self) -> &'static str {
        "cache_anchored_surfaces_final"
    }

    fn run(&self, context: &PackageMatchContext<'_>, state: &mut PackageMatchState) {
        // Ownership / force-externalize passes can append accepted
        // external-import attributions after the versioned matcher resolved
        // surfaces from the initial concrete matches. Re-resolve
        // cache-anchored surfaces over the now-complete attribution set so
        // every publicly importable specifier gets a generation surface.
        append_cache_anchored_package_surfaces(
            &mut state.package_report.surfaces,
            &state.package_report.attributions,
            context.package_sources,
            context.package_filter,
        );
    }
}

fn empty_versioned_package_match_report() -> VersionedPackageMatchReport {
    VersionedPackageMatchReport {
        attributions: Vec::new(),
        surfaces: Vec::new(),
        matches: Vec::new(),
        version_matches: Vec::new(),
        audit: AuditReport::default(),
    }
}

/// Builds per-module function fingerprints from raw input rows using the same
/// function-axis extractor that powers the cascade package-source index.
fn fingerprints_from_rows(
    rows: &InputRows,
    package_filter: Option<&BTreeSet<String>>,
    excluded_modules: &BTreeSet<ModuleId>,
    only_weak_package_sources: bool,
) -> BTreeMap<ModuleId, Vec<FunctionFingerprint>> {
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
    fingerprints_by_module: &BTreeMap<ModuleId, Vec<FunctionFingerprint>>,
    package_sources: &[PackageSource],
) -> CascadeMatchReport {
    let modules_by_id = rows
        .modules
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let mut grouped_fingerprints = BTreeMap::<
        (Option<String>, Option<String>),
        BTreeMap<ModuleId, Vec<FunctionFingerprint>>,
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
