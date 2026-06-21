use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use reverts_graph::{FunctionExtractor, extract_import_specifiers};
use reverts_input::{InputRows, ModuleDependencyTarget, PackageAttributionInput};
use reverts_ir::{AxisKind, FunctionFingerprint, ModuleId, ModuleKind};
use reverts_observe::AuditReport;

use crate::index::{
    module_match_fingerprint, package_module_source_quality, package_source_fingerprint,
};
use crate::model::{
    ModuleMatchStrategy, PackageMatch, PackageMatchingPipelineReport, PackageModuleSourceQuality,
    PackageSource, VersionedPackageMatchReport,
};
use crate::proof::concrete_source::unmatched_package_scope;
use crate::source::cache_surfaces::append_cache_anchored_package_surfaces;
use crate::strategy::{
    self, CascadeMatchReport, match_structural_bags_with_excluded_modules, match_with_cascade,
};
use crate::{
    GraphNeighborhoodEvidence, VersionedPackageMatcher, graph_neighborhood_support,
    has_accepted_external_attribution, has_direct_neighborhood_package_contradiction, ownership,
    ownership_by_module,
};

const CASCADE_MATCHED_MODULE_SOURCE_LIMIT: usize = 8;
const CASCADE_PIPELINE_SOURCE_LIMIT: usize = 4096;
const CASCADE_SOURCE_GROUP_LIMIT: usize = 128;
const ANONYMOUS_CASCADE_PACKAGE_VERSION_LIMIT: usize = 64;
const ANONYMOUS_CASCADE_MIN_STRING_ANCHORS: usize = 3;
const ANONYMOUS_CASCADE_MIN_FUNCTION_SIGNATURES: usize = 1;
const ANONYMOUS_AXIS_SOURCE_LIMIT: usize = 8_192;
const ANONYMOUS_AXIS_SOURCE_BUDGET: usize = 256;
const ANONYMOUS_AXIS_SOURCE_MAX_BYTES: usize = 64 * 1024;
const ANONYMOUS_AXIS_TOKEN_VERSION_LIMIT: usize = 20;
const ANONYMOUS_AXIS_TOKEN_SOURCE_LIMIT: usize = 20;
const ANONYMOUS_AXIS_MIN_SCORE: usize = 12;
const ANONYMOUS_AXIS_MIN_MARGIN: usize = 8;
const ANONYMOUS_ANCHOR_MIN_SHARED: usize = 2;
const ANONYMOUS_ANCHOR_MIN_WEIGHT: f64 = 4.0;
const ANONYMOUS_ANCHOR_MIN_MARGIN: f64 = 2.0;
const ANONYMOUS_ANCHOR_MAX_DF: usize = 24;
const PACKAGE_GRAPH_SOURCE_LIMIT: usize = 8_192;
const PACKAGE_GRAPH_SOURCE_MAX_BYTES: usize = 64 * 1024;
const PACKAGE_GRAPH_MIN_MATCHED_EDGES: usize = 2;
const PACKAGE_GRAPH_MIN_COVERAGE_PERCENT: usize = 100;
const ANONYMOUS_DEPENDENCY_NEIGHBOR_MIN_OWNED: usize = 1;
const ANONYMOUS_DEPENDENCY_OUTGOING_ONLY_MAX_SOURCE_BYTES: usize = 2_000;
const ANONYMOUS_DEPENDENCY_CLUSTER_MIN_OWNED: usize = 3;
const ANONYMOUS_DEPENDENCY_CLUSTER_DOMINANCE_PERCENT: usize = 80;
const ANONYMOUS_DEPENDENCY_CLUSTER_MAX_SOURCE_BYTES: usize = 1_000;
const PACKAGE_SOURCE_EXTENSIONS: &[&str] = &["js", "jsx", "mjs", "cjs", "ts", "tsx"];

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
    run_package_match_pass(AnonymousExactSourcePass, &context, &mut state, &mut timing);
    run_package_match_pass(
        PackageGraphNeighborhoodPass,
        &context,
        &mut state,
        &mut timing,
    );
    run_package_match_pass(AnonymousFunctionAxisPass, &context, &mut state, &mut timing);
    run_package_match_pass(AnonymousAnchorAxisPass, &context, &mut state, &mut timing);
    run_package_match_pass(
        PackageGraphNeighborhoodPass,
        &context,
        &mut state,
        &mut timing,
    );
    run_package_match_pass(
        AnonymousDependencyNeighborhoodPass,
        &context,
        &mut state,
        &mut timing,
    );
    run_package_match_pass(
        AnonymousDependencyClusterPass,
        &context,
        &mut state,
        &mut timing,
    );
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
    run_package_match_pass(
        ProvenExternalImportTargetsPass,
        &context,
        &mut state,
        &mut timing,
    );
    run_package_match_pass(ImportablePass, &context, &mut state, &mut timing);
    run_package_match_pass(AnonymousImportablePass, &context, &mut state, &mut timing);
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

struct AnonymousExactSourcePass;
struct PackageGraphNeighborhoodPass;
struct AnonymousFunctionAxisPass;
struct AnonymousAnchorAxisPass;
struct AnonymousDependencyNeighborhoodPass;
struct AnonymousDependencyClusterPass;

impl PackageMatchPass for AnonymousExactSourcePass {
    fn name(&self) -> &'static str {
        "anonymous_exact_source"
    }

    fn run(&self, context: &PackageMatchContext<'_>, state: &mut PackageMatchState) {
        if context.package_sources.is_empty()
            || context.package_sources.len() > ANONYMOUS_AXIS_SOURCE_LIMIT
        {
            return;
        }
        let mut sources_by_hash = BTreeMap::<String, BTreeSet<usize>>::new();
        for (source_index, source) in context.package_sources.iter().enumerate() {
            let Ok(fingerprint) = package_source_fingerprint(source) else {
                continue;
            };
            for hash in fingerprint.normalized_source_hashes {
                sources_by_hash
                    .entry(hash)
                    .or_default()
                    .insert(source_index);
            }
        }
        if sources_by_hash.is_empty() {
            return;
        }
        let already_matched = state
            .package_report
            .matches
            .iter()
            .map(|package_match| package_match.module_id)
            .collect::<BTreeSet<_>>();
        for module in &context.rows.modules {
            if already_matched.contains(&module.id)
                || !is_anonymous_bundle_package_candidate(context.rows, module)
            {
                continue;
            }
            let Some(slice) = context.rows.module_source_slice(module.id) else {
                continue;
            };
            if package_module_source_quality(module, slice.source_file_path, slice.source)
                == PackageModuleSourceQuality::Invalid
            {
                continue;
            }
            let Ok(module_fingerprint) =
                module_match_fingerprint(module, slice.source_file_path, slice.source)
            else {
                continue;
            };
            let candidates = module_fingerprint
                .normalized_source_hashes
                .iter()
                .filter_map(|hash| sources_by_hash.get(hash))
                .flatten()
                .copied()
                .map(|source_index| &context.package_sources[source_index])
                .map(|source| {
                    (
                        source.package_name.as_str(),
                        source.package_version.as_str(),
                        source.source_path.as_str(),
                        source.export_specifier.as_str(),
                    )
                })
                .collect::<BTreeSet<_>>();
            if candidates.len() != 1 {
                continue;
            }
            let Some((package_name, package_version, source_path, export_specifier)) =
                candidates.into_iter().next()
            else {
                continue;
            };
            state.package_report.matches.push(PackageMatch {
                module_id: module.id,
                package_name: package_name.to_string(),
                package_version: package_version.to_string(),
                export_specifier: export_specifier.to_string(),
                source_path: format!("anonymous-exact-source:{source_path}"),
                normalized_source_hash: module_fingerprint.normalized_source_hash,
                strategy: ModuleMatchStrategy::NormalizedSourceHash,
                function_signature_matches: 0,
                string_anchor_matches: 0,
                external_importable: false,
            });
        }
    }
}

impl PackageMatchPass for PackageGraphNeighborhoodPass {
    fn name(&self) -> &'static str {
        "package_graph_neighborhood"
    }

    fn run(&self, context: &PackageMatchContext<'_>, state: &mut PackageMatchState) {
        if context.package_sources.is_empty()
            || context.package_sources.len() > PACKAGE_GRAPH_SOURCE_LIMIT
            || context.rows.dependencies.is_empty()
        {
            return;
        }
        let module_outgoing = module_dependency_graph(context.rows);
        let module_incoming = reverse_module_dependency_graph(&module_outgoing);
        if module_outgoing.is_empty() && module_incoming.is_empty() {
            return;
        }
        let package_graph = build_package_source_graph(context.package_sources);
        if package_graph.outgoing.is_empty() && package_graph.incoming.is_empty() {
            return;
        }

        let already_accepted =
            crate::accepted_external_modules(context.rows, &state.package_report);
        let mut matched_modules = state
            .package_report
            .matches
            .iter()
            .map(|package_match| package_match.module_id)
            .collect::<BTreeSet<_>>();
        let mut round = 0usize;
        loop {
            round += 1;
            let seed_matches =
                package_source_seed_matches(&state.package_report.matches, context.package_sources);
            if seed_matches.is_empty() {
                return;
            }
            let support = graph_neighborhood_support(
                &module_outgoing,
                &module_incoming,
                &package_graph.outgoing,
                &package_graph.incoming,
                &seed_matches,
            );
            let ownership = ownership_by_module(context.rows, &state.package_report);
            let modules_by_id = context
                .rows
                .modules
                .iter()
                .map(|module| (module.id, module))
                .collect::<BTreeMap<_, _>>();
            let mut promoted = Vec::<PackageMatch>::new();
            for (module_id, candidates) in &support {
                if already_accepted.contains(module_id) || matched_modules.contains(module_id) {
                    continue;
                }
                let Some(module) = modules_by_id.get(module_id).copied() else {
                    continue;
                };
                let Some((candidate, evidence, runner_up_edges)) =
                    best_package_graph_candidate(module, candidates)
                else {
                    continue;
                };
                if evidence.matched_edges < PACKAGE_GRAPH_MIN_MATCHED_EDGES
                    || evidence.matched_edges * 100
                        < evidence.known_edges * PACKAGE_GRAPH_MIN_COVERAGE_PERCENT
                    || runner_up_edges >= evidence.matched_edges
                    || !package_graph_candidate_allowed(context.rows, module, candidate)
                    || has_direct_neighborhood_package_contradiction(
                        context.rows,
                        *module_id,
                        candidate.package_name.as_str(),
                        &ownership,
                    )
                {
                    continue;
                }
                promoted.push(PackageMatch {
                    module_id: *module_id,
                    package_name: candidate.package_name.clone(),
                    package_version: candidate.package_version.clone(),
                    export_specifier: candidate.package_name.clone(),
                    source_path: format!(
                        "package-graph-neighborhood:{}@{}:{}:matched={}:known={}:round={round}",
                        candidate.package_name,
                        candidate.package_version,
                        candidate.source_path,
                        evidence.matched_edges,
                        evidence.known_edges,
                    ),
                    normalized_source_hash: String::new(),
                    strategy: ModuleMatchStrategy::PackageGraphNeighborhoodOwnership,
                    function_signature_matches: evidence.matched_edges,
                    string_anchor_matches: evidence.known_edges,
                    external_importable: false,
                });
            }
            if promoted.is_empty() {
                break;
            }
            for package_match in promoted {
                matched_modules.insert(package_match.module_id);
                state.package_report.matches.push(package_match);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PackageSourceNode {
    package_name: String,
    package_version: String,
    source_path: String,
}

struct PackageSourceGraph {
    outgoing: BTreeMap<PackageSourceNode, BTreeSet<PackageSourceNode>>,
    incoming: BTreeMap<PackageSourceNode, BTreeSet<PackageSourceNode>>,
}

fn build_package_source_graph(package_sources: &[PackageSource]) -> PackageSourceGraph {
    let sources_by_key = package_sources
        .iter()
        .map(|source| {
            (
                PackageSourceNode {
                    package_name: source.package_name.clone(),
                    package_version: source.package_version.clone(),
                    source_path: source.source_path.clone(),
                },
                source,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let known_paths = sources_by_key
        .keys()
        .map(|key| {
            (
                key.package_name.as_str(),
                key.package_version.as_str(),
                key.source_path.as_str(),
            )
        })
        .collect::<BTreeSet<_>>();
    let mut outgoing = BTreeMap::<PackageSourceNode, BTreeSet<PackageSourceNode>>::new();
    for (source_key, source) in &sources_by_key {
        let deps = if source.source.len() > PACKAGE_GRAPH_SOURCE_MAX_BYTES {
            BTreeSet::new()
        } else {
            extract_import_specifiers(source.source.as_str())
                .into_iter()
                .filter_map(|specifier| {
                    resolve_package_source_import(source_key, specifier.as_str(), &known_paths)
                })
                .collect::<BTreeSet<_>>()
        };
        outgoing.insert(source_key.clone(), deps);
    }
    let incoming = reverse_package_source_graph(&outgoing);
    PackageSourceGraph { outgoing, incoming }
}

fn module_dependency_graph(rows: &InputRows) -> BTreeMap<ModuleId, BTreeSet<ModuleId>> {
    let mut outgoing = rows
        .modules
        .iter()
        .map(|module| (module.id, BTreeSet::new()))
        .collect::<BTreeMap<_, _>>();
    for dependency in &rows.dependencies {
        if let ModuleDependencyTarget::Module(target) = dependency.target {
            outgoing
                .entry(dependency.from_module_id)
                .or_default()
                .insert(target);
        }
    }
    outgoing
}

fn reverse_module_dependency_graph(
    graph: &BTreeMap<ModuleId, BTreeSet<ModuleId>>,
) -> BTreeMap<ModuleId, BTreeSet<ModuleId>> {
    let mut incoming = BTreeMap::<ModuleId, BTreeSet<ModuleId>>::new();
    for (source, targets) in graph {
        incoming.entry(*source).or_default();
        for target in targets {
            incoming.entry(*target).or_default().insert(*source);
        }
    }
    incoming
}

fn reverse_package_source_graph(
    graph: &BTreeMap<PackageSourceNode, BTreeSet<PackageSourceNode>>,
) -> BTreeMap<PackageSourceNode, BTreeSet<PackageSourceNode>> {
    let mut incoming = BTreeMap::<PackageSourceNode, BTreeSet<PackageSourceNode>>::new();
    for (source, targets) in graph {
        incoming.entry(source.clone()).or_default();
        for target in targets {
            incoming
                .entry(target.clone())
                .or_default()
                .insert(source.clone());
        }
    }
    incoming
}

fn package_source_seed_matches(
    matches: &[PackageMatch],
    package_sources: &[PackageSource],
) -> BTreeMap<ModuleId, PackageSourceNode> {
    let source_keys = package_sources
        .iter()
        .map(|source| {
            (
                (
                    source.package_name.as_str(),
                    source.package_version.as_str(),
                    source.source_path.as_str(),
                ),
                PackageSourceNode {
                    package_name: source.package_name.clone(),
                    package_version: source.package_version.clone(),
                    source_path: source.source_path.clone(),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut seeds = BTreeMap::new();
    for package_match in matches {
        let Some(source_path) = concrete_package_graph_source_path(package_match) else {
            continue;
        };
        let Some(source_key) = source_keys.get(&(
            package_match.package_name.as_str(),
            package_match.package_version.as_str(),
            source_path,
        )) else {
            continue;
        };
        seeds.insert(package_match.module_id, source_key.clone());
    }
    seeds
}

fn concrete_package_graph_source_path(package_match: &PackageMatch) -> Option<&str> {
    if let Some(source_path) = package_match
        .source_path
        .strip_prefix("anonymous-exact-source:")
    {
        return Some(source_path);
    }
    if let Some(rest) = package_match
        .source_path
        .strip_prefix("anonymous-function-axis-source:")
    {
        let prefix = format!(
            "{}@{}:",
            package_match.package_name, package_match.package_version
        );
        let rest = rest.strip_prefix(prefix.as_str())?;
        return rest.split(':').next();
    }
    if package_match.source_path.contains(':') {
        return None;
    }
    Some(package_match.source_path.as_str())
}

fn best_package_graph_candidate<'a>(
    module: &reverts_input::ModuleInput,
    candidates: &'a BTreeMap<PackageSourceNode, GraphNeighborhoodEvidence>,
) -> Option<(&'a PackageSourceNode, GraphNeighborhoodEvidence, usize)> {
    let mut ranked = candidates
        .iter()
        .filter(|(candidate, _evidence)| package_hint_allows_candidate(module, candidate))
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .1
            .matched_edges
            .cmp(&left.1.matched_edges)
            .then_with(|| right.1.known_edges.cmp(&left.1.known_edges))
            .then_with(|| left.0.cmp(right.0))
    });
    let (candidate, evidence) = ranked.first().copied()?;
    let runner_up_edges = ranked
        .get(1)
        .map_or(0, |(_candidate, evidence)| evidence.matched_edges);
    Some((candidate, *evidence, runner_up_edges))
}

fn package_graph_candidate_allowed(
    rows: &InputRows,
    module: &reverts_input::ModuleInput,
    candidate: &PackageSourceNode,
) -> bool {
    if !package_hint_allows_candidate(module, candidate) {
        return false;
    }
    module_package_hint(module).is_some() || is_anonymous_bundle_package_candidate(rows, module)
}

fn package_hint_allows_candidate(
    module: &reverts_input::ModuleInput,
    candidate: &PackageSourceNode,
) -> bool {
    let Some((package_name, package_version)) = module_package_hint(module) else {
        return true;
    };
    package_name == candidate.package_name
        && package_version.is_none_or(|version| version == candidate.package_version)
}

fn resolve_package_source_import(
    importer: &PackageSourceNode,
    specifier: &str,
    known_paths: &BTreeSet<(&str, &str, &str)>,
) -> Option<PackageSourceNode> {
    let specifier = specifier.split(['?', '#']).next().unwrap_or(specifier);
    let candidate_root = if specifier.starts_with('.') {
        let base_dir = importer
            .source_path
            .rsplit_once('/')
            .map_or("", |(dir, _file)| dir);
        let joined = if base_dir.is_empty() {
            specifier.to_string()
        } else {
            format!("{base_dir}/{specifier}")
        };
        normalize_package_source_path(joined.as_str())?
    } else if specifier == importer.package_name {
        "index".to_string()
    } else if let Some(stripped) = specifier
        .strip_prefix(importer.package_name.as_str())
        .and_then(|rest| rest.strip_prefix('/'))
    {
        stripped.to_string()
    } else {
        return None;
    };
    package_source_path_candidates(candidate_root.as_str())
        .into_iter()
        .find(|candidate| {
            known_paths.contains(&(
                importer.package_name.as_str(),
                importer.package_version.as_str(),
                candidate.as_str(),
            ))
        })
        .map(|source_path| PackageSourceNode {
            package_name: importer.package_name.clone(),
            package_version: importer.package_version.clone(),
            source_path,
        })
}

fn normalize_package_source_path(path: &str) -> Option<String> {
    let mut parts = Vec::<&str>::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

fn package_source_path_candidates(path: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    candidates.push(path.to_string());
    if let Some((stem, _extension)) = path.rsplit_once('.') {
        for ext in PACKAGE_SOURCE_EXTENSIONS {
            candidates.push(format!("{stem}.{ext}"));
        }
    } else {
        for ext in PACKAGE_SOURCE_EXTENSIONS {
            candidates.push(format!("{path}.{ext}"));
        }
        for ext in PACKAGE_SOURCE_EXTENSIONS {
            candidates.push(format!("{path}/index.{ext}"));
        }
    }
    candidates
}

impl PackageMatchPass for AnonymousFunctionAxisPass {
    fn name(&self) -> &'static str {
        "anonymous_function_axis"
    }

    fn run(&self, context: &PackageMatchContext<'_>, state: &mut PackageMatchState) {
        if context.package_sources.is_empty() {
            return;
        }
        let already_matched = state
            .package_report
            .matches
            .iter()
            .map(|package_match| package_match.module_id)
            .collect::<BTreeSet<_>>();
        let anonymous_modules = context
            .rows
            .modules
            .iter()
            .filter(|module| {
                !already_matched.contains(&module.id)
                    && is_anonymous_bundle_package_candidate(context.rows, module)
            })
            .collect::<Vec<_>>();
        if anonymous_modules.is_empty() {
            return;
        }

        let need_source_file_seeds = !context.rows.dependencies.is_empty();
        let axis_index = package_function_axis_index(
            context.rows,
            context.package_sources,
            need_source_file_seeds,
        );
        if axis_index.token_versions.is_empty() {
            return;
        }

        let module_sources = anonymous_modules
            .into_iter()
            .filter_map(|module| {
                let slice = context.rows.module_source_slice(module.id)?;
                (slice.source.len() <= ANONYMOUS_AXIS_SOURCE_MAX_BYTES)
                    .then_some((module.id, slice.source))
            })
            .collect::<Vec<_>>();
        let mut matches = anonymous_axis_matches_parallel(
            module_sources.as_slice(),
            &axis_index,
            need_source_file_seeds,
        );
        matches.sort_by(|left, right| left.module_id.cmp(&right.module_id));
        state.package_report.matches.extend(matches);
    }
}

struct FunctionAxisIndex {
    token_versions: BTreeMap<FunctionAxisToken, BTreeSet<(String, String)>>,
    token_sources: BTreeMap<FunctionAxisToken, BTreeSet<PackageSourceNode>>,
}

fn anonymous_axis_matches_parallel(
    module_sources: &[(ModuleId, &str)],
    axis_index: &FunctionAxisIndex,
    need_source_file_seeds: bool,
) -> Vec<PackageMatch> {
    if module_sources.is_empty() {
        return Vec::new();
    }
    let worker_count = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(module_sources.len())
        .max(1);
    let chunk_size = module_sources.len().div_ceil(worker_count);
    std::thread::scope(|scope| {
        let handles = module_sources
            .chunks(chunk_size)
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .filter_map(|(module_id, source)| {
                            anonymous_axis_match_for_module(
                                *module_id,
                                source,
                                axis_index,
                                need_source_file_seeds,
                            )
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        let mut out = Vec::new();
        for handle in handles {
            match handle.join() {
                Ok(mut matches) => out.append(&mut matches),
                Err(payload) => std::panic::resume_unwind(payload),
            }
        }
        out
    })
}

fn anonymous_axis_match_for_module(
    module_id: ModuleId,
    source: &str,
    axis_index: &FunctionAxisIndex,
    need_source_file_seeds: bool,
) -> Option<PackageMatch> {
    let tokens = FunctionExtractor::fingerprint(module_id, source)
        .iter()
        .flat_map(function_axis_tokens)
        .collect::<BTreeSet<_>>();
    if tokens.is_empty() {
        return None;
    }
    let mut scores = BTreeMap::<(String, String), usize>::new();
    for token in &tokens {
        let Some(versions) = axis_index.token_versions.get(token) else {
            continue;
        };
        if versions.len() > ANONYMOUS_AXIS_TOKEN_VERSION_LIMIT {
            continue;
        }
        let weight = token.axis_weight();
        for (package_name, package_version) in versions {
            *scores
                .entry((package_name.clone(), package_version.clone()))
                .or_default() += weight;
        }
    }
    let mut ranked = scores.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    let ((package_name, package_version), score) = ranked.first()?;
    let runner_up = ranked.get(1).map_or(0, |(_key, score)| *score);
    if *score < ANONYMOUS_AXIS_MIN_SCORE
        || score.saturating_sub(runner_up) < ANONYMOUS_AXIS_MIN_MARGIN
    {
        return None;
    }
    let source_candidate = need_source_file_seeds
        .then(|| best_axis_source_candidate(&tokens, axis_index, package_name, package_version))
        .flatten();
    let source_path = source_candidate.map_or_else(
        || {
            format!(
                "anonymous-function-axis:{package_name}@{package_version}:score={score}:runner_up={runner_up}"
            )
        },
        |(source, source_score, source_runner_up)| {
            format!(
                "anonymous-function-axis-source:{package_name}@{package_version}:{}:score={score}:runner_up={runner_up}:source_score={source_score}:source_runner_up={source_runner_up}",
                source.source_path,
            )
        },
    );
    Some(PackageMatch {
        module_id,
        package_name: package_name.clone(),
        package_version: package_version.clone(),
        export_specifier: package_name.clone(),
        source_path,
        normalized_source_hash: String::new(),
        strategy: ModuleMatchStrategy::AggregateFunctionSignatureAndStringAnchors,
        function_signature_matches: *score,
        string_anchor_matches: 0,
        external_importable: false,
    })
}

impl PackageMatchPass for AnonymousAnchorAxisPass {
    fn name(&self) -> &'static str {
        "anonymous_anchor_axis"
    }

    fn run(&self, context: &PackageMatchContext<'_>, state: &mut PackageMatchState) {
        if context.package_sources.is_empty()
            || context.package_sources.len() > ANONYMOUS_AXIS_SOURCE_LIMIT
        {
            return;
        }
        let mut packages_by_anchor = BTreeMap::<String, BTreeSet<(String, String)>>::new();
        let mut anchor_df = BTreeMap::<String, usize>::new();
        for source in context
            .package_sources
            .iter()
            .filter(|source| source.source.len() <= ANONYMOUS_AXIS_SOURCE_MAX_BYTES)
        {
            let Ok(fingerprint) = package_source_fingerprint(source) else {
                continue;
            };
            let package_key = (source.package_name.clone(), source.package_version.clone());
            for anchor in fingerprint.string_anchors {
                packages_by_anchor
                    .entry(anchor.clone())
                    .or_default()
                    .insert(package_key.clone());
            }
        }
        packages_by_anchor.retain(|anchor, packages| {
            anchor_df.insert(anchor.clone(), packages.len());
            !packages.is_empty() && packages.len() <= ANONYMOUS_ANCHOR_MAX_DF
        });
        if packages_by_anchor.is_empty() {
            return;
        }
        let package_count = context
            .package_sources
            .iter()
            .map(|source| {
                (
                    source.package_name.as_str(),
                    source.package_version.as_str(),
                )
            })
            .collect::<BTreeSet<_>>()
            .len()
            .max(1) as f64;
        let already_matched = state
            .package_report
            .matches
            .iter()
            .map(|package_match| package_match.module_id)
            .collect::<BTreeSet<_>>();
        for module in &context.rows.modules {
            if already_matched.contains(&module.id)
                || !is_anonymous_bundle_package_candidate(context.rows, module)
            {
                continue;
            }
            let Some(slice) = context.rows.module_source_slice(module.id) else {
                continue;
            };
            if package_module_source_quality(module, slice.source_file_path, slice.source)
                == PackageModuleSourceQuality::Invalid
            {
                continue;
            }
            let Ok(module_fingerprint) =
                module_match_fingerprint(module, slice.source_file_path, slice.source)
            else {
                continue;
            };
            let mut scores = BTreeMap::<(String, String), (usize, f64)>::new();
            for anchor in &module_fingerprint.string_anchors {
                let Some(packages) = packages_by_anchor.get(anchor) else {
                    continue;
                };
                let df = anchor_df
                    .get(anchor)
                    .copied()
                    .unwrap_or(packages.len())
                    .max(1) as f64;
                let weight = (package_count / df).ln().max(0.0);
                if weight <= f64::EPSILON {
                    continue;
                }
                for package in packages {
                    let entry = scores.entry(package.clone()).or_default();
                    entry.0 += 1;
                    entry.1 += weight;
                }
            }
            let mut ranked = scores.into_iter().collect::<Vec<_>>();
            ranked.sort_by(|left, right| {
                right
                    .1
                    .1
                    .total_cmp(&left.1.1)
                    .then_with(|| right.1.0.cmp(&left.1.0))
                    .then_with(|| left.0.cmp(&right.0))
            });
            let Some(((package_name, package_version), (shared, score))) = ranked.first() else {
                continue;
            };
            let runner_up = ranked.get(1).map_or(0.0, |(_, (_, score))| *score);
            if *shared < ANONYMOUS_ANCHOR_MIN_SHARED
                || *score < ANONYMOUS_ANCHOR_MIN_WEIGHT
                || *score - runner_up < ANONYMOUS_ANCHOR_MIN_MARGIN
            {
                continue;
            }
            state.package_report.matches.push(PackageMatch {
                module_id: module.id,
                package_name: package_name.clone(),
                package_version: package_version.clone(),
                export_specifier: package_name.clone(),
                source_path: format!(
                    "anonymous-anchor-axis:{package_name}@{package_version}:anchors={shared}:score={score:.1}:runner_up={runner_up:.1}"
                ),
                normalized_source_hash: String::new(),
                strategy: ModuleMatchStrategy::AggregateStringAnchorSimilarity,
                function_signature_matches: 0,
                string_anchor_matches: *shared,
                external_importable: false,
            });
        }
    }
}

impl PackageMatchPass for AnonymousDependencyNeighborhoodPass {
    fn name(&self) -> &'static str {
        "anonymous_dependency_neighborhood"
    }

    fn run(&self, context: &PackageMatchContext<'_>, state: &mut PackageMatchState) {
        if context.rows.dependencies.is_empty() {
            return;
        }
        let outgoing = module_dependency_graph(context.rows);
        let incoming = reverse_module_dependency_graph(&outgoing);
        let modules_by_id = context
            .rows
            .modules
            .iter()
            .map(|module| (module.id, module))
            .collect::<BTreeMap<_, _>>();
        let already_accepted =
            crate::accepted_external_modules(context.rows, &state.package_report);
        let mut matched_modules = state
            .package_report
            .matches
            .iter()
            .map(|package_match| package_match.module_id)
            .collect::<BTreeSet<_>>();
        let mut round = 0usize;
        loop {
            round += 1;
            let ownership = ownership_by_module(context.rows, &state.package_report);
            let mut promoted = Vec::<PackageMatch>::new();
            for module in &context.rows.modules {
                if already_accepted.contains(&module.id)
                    || matched_modules.contains(&module.id)
                    || !is_anonymous_bundle_package_candidate(context.rows, module)
                {
                    continue;
                }
                let Some((package_name, package_version, evidence)) =
                    anonymous_dependency_neighborhood_candidate(
                        context.rows,
                        module.id,
                        &outgoing,
                        &incoming,
                        &ownership,
                    )
                else {
                    continue;
                };
                if evidence.incoming_owned_neighbors == 0
                    && has_direct_neighborhood_package_contradiction(
                        context.rows,
                        module.id,
                        package_name.as_str(),
                        &ownership,
                    )
                {
                    continue;
                }
                promoted.push(PackageMatch {
                    module_id: module.id,
                    package_name: package_name.clone(),
                    package_version: package_version.clone(),
                    export_specifier: package_name.clone(),
                    source_path: format!(
                        "anonymous-dependency-neighborhood:{package_name}@{package_version}:owned={}:incoming={}:outgoing={}:round={round}",
                        evidence.owned_neighbors,
                        evidence.incoming_owned_neighbors,
                        evidence.outgoing_owned_neighbors,
                    ),
                    normalized_source_hash: String::new(),
                    strategy: ModuleMatchStrategy::DependencyClosureOwnership,
                    function_signature_matches: evidence.owned_neighbors,
                    string_anchor_matches: evidence.incoming_owned_neighbors,
                    external_importable: false,
                });
            }
            if promoted.is_empty() {
                break;
            }
            for package_match in promoted {
                if modules_by_id.contains_key(&package_match.module_id) {
                    matched_modules.insert(package_match.module_id);
                    state.package_report.matches.push(package_match);
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AnonymousDependencyNeighborhoodEvidence {
    owned_neighbors: usize,
    incoming_owned_neighbors: usize,
    outgoing_owned_neighbors: usize,
}

fn anonymous_dependency_neighborhood_candidate(
    rows: &InputRows,
    module_id: ModuleId,
    outgoing: &BTreeMap<ModuleId, BTreeSet<ModuleId>>,
    incoming: &BTreeMap<ModuleId, BTreeSet<ModuleId>>,
    ownership: &BTreeMap<ModuleId, (String, String)>,
) -> Option<(String, String, AnonymousDependencyNeighborhoodEvidence)> {
    let mut counts = BTreeMap::<(String, String), (usize, usize, usize)>::new();
    let mut incoming_counts = BTreeMap::<(String, String), usize>::new();
    let mut owned_neighbors = 0usize;
    for neighbor in incoming.get(&module_id).into_iter().flatten() {
        if let Some((package_name, package_version)) = ownership.get(neighbor) {
            owned_neighbors += 1;
            let entry = counts
                .entry((package_name.clone(), package_version.clone()))
                .or_default();
            entry.0 += 1;
            entry.1 += 1;
            *incoming_counts
                .entry((package_name.clone(), package_version.clone()))
                .or_default() += 1;
        }
    }
    for neighbor in outgoing.get(&module_id).into_iter().flatten() {
        if let Some((package_name, package_version)) = ownership.get(neighbor) {
            owned_neighbors += 1;
            let entry = counts
                .entry((package_name.clone(), package_version.clone()))
                .or_default();
            entry.0 += 1;
            entry.2 += 1;
        }
    }
    if owned_neighbors < ANONYMOUS_DEPENDENCY_NEIGHBOR_MIN_OWNED || counts.len() != 1 {
        if incoming_counts.len() != 1 {
            return None;
        }
        let ((package_name, package_version), incoming_count) =
            incoming_counts.into_iter().next()?;
        let outgoing_count = counts
            .get(&(package_name.clone(), package_version.clone()))
            .map_or(0, |(_, _, outgoing_count)| *outgoing_count);
        return Some((
            package_name,
            package_version,
            AnonymousDependencyNeighborhoodEvidence {
                owned_neighbors: incoming_count + outgoing_count,
                incoming_owned_neighbors: incoming_count,
                outgoing_owned_neighbors: outgoing_count,
            },
        ));
    }
    let ((package_name, package_version), (same_package, incoming_count, outgoing_count)) =
        counts.into_iter().next()?;
    if same_package != owned_neighbors {
        return None;
    }
    if incoming_count == 0 {
        let direct_outgoing = outgoing.get(&module_id).map_or(0, BTreeSet::len);
        let source_len = rows
            .module_source_slice(module_id)
            .map_or(usize::MAX, |slice| slice.source.len());
        if direct_outgoing != outgoing_count
            || source_len > ANONYMOUS_DEPENDENCY_OUTGOING_ONLY_MAX_SOURCE_BYTES
        {
            return None;
        }
    }
    Some((
        package_name,
        package_version,
        AnonymousDependencyNeighborhoodEvidence {
            owned_neighbors,
            incoming_owned_neighbors: incoming_count,
            outgoing_owned_neighbors: outgoing_count,
        },
    ))
}

impl PackageMatchPass for AnonymousDependencyClusterPass {
    fn name(&self) -> &'static str {
        "anonymous_dependency_cluster"
    }

    fn run(&self, context: &PackageMatchContext<'_>, state: &mut PackageMatchState) {
        if context.rows.dependencies.is_empty() {
            return;
        }
        let ownership = ownership_by_module(context.rows, &state.package_report);
        let already_accepted =
            crate::accepted_external_modules(context.rows, &state.package_report);
        let mut matched_modules = state
            .package_report
            .matches
            .iter()
            .map(|package_match| package_match.module_id)
            .collect::<BTreeSet<_>>();
        let modules_by_id = context
            .rows
            .modules
            .iter()
            .map(|module| (module.id, module))
            .collect::<BTreeMap<_, _>>();
        for component in dependency_components(context.rows) {
            let mut counts = BTreeMap::<(String, String), usize>::new();
            for module_id in &component {
                if let Some((package_name, package_version)) = ownership.get(module_id) {
                    *counts
                        .entry((package_name.clone(), package_version.clone()))
                        .or_default() += 1;
                }
            }
            let owned_total = counts.values().sum::<usize>();
            if owned_total < ANONYMOUS_DEPENDENCY_CLUSTER_MIN_OWNED {
                continue;
            }
            let Some(((package_name, package_version), dominant_count)) = counts
                .into_iter()
                .max_by(|left, right| left.1.cmp(&right.1).then_with(|| right.0.cmp(&left.0)))
            else {
                continue;
            };
            if dominant_count * 100 < owned_total * ANONYMOUS_DEPENDENCY_CLUSTER_DOMINANCE_PERCENT {
                continue;
            }
            for module_id in &component {
                if already_accepted.contains(module_id) || matched_modules.contains(module_id) {
                    continue;
                }
                let Some(module) = modules_by_id.get(module_id).copied() else {
                    continue;
                };
                if !is_anonymous_bundle_package_candidate(context.rows, module) {
                    continue;
                }
                let Some(slice) = context.rows.module_source_slice(*module_id) else {
                    continue;
                };
                if slice.source.len() > ANONYMOUS_DEPENDENCY_CLUSTER_MAX_SOURCE_BYTES {
                    continue;
                }
                if has_direct_neighborhood_package_contradiction(
                    context.rows,
                    *module_id,
                    package_name.as_str(),
                    &ownership,
                ) {
                    continue;
                }
                matched_modules.insert(*module_id);
                state.package_report.matches.push(PackageMatch {
                    module_id: *module_id,
                    package_name: package_name.clone(),
                    package_version: package_version.clone(),
                    export_specifier: package_name.clone(),
                    source_path: format!(
                        "anonymous-dependency-cluster:{package_name}@{package_version}:owned={dominant_count}/{owned_total}:component_size={}",
                        component.len(),
                    ),
                    normalized_source_hash: String::new(),
                    strategy: ModuleMatchStrategy::DependencyClosureOwnership,
                    function_signature_matches: dominant_count,
                    string_anchor_matches: owned_total,
                    external_importable: false,
                });
            }
        }
    }
}

fn dependency_components(rows: &InputRows) -> Vec<BTreeSet<ModuleId>> {
    let mut adjacency = BTreeMap::<ModuleId, BTreeSet<ModuleId>>::new();
    for module in &rows.modules {
        adjacency.entry(module.id).or_default();
    }
    for dependency in &rows.dependencies {
        if let ModuleDependencyTarget::Module(target) = dependency.target {
            adjacency
                .entry(dependency.from_module_id)
                .or_default()
                .insert(target);
            adjacency
                .entry(target)
                .or_default()
                .insert(dependency.from_module_id);
        }
    }
    let seeds = adjacency.keys().copied().collect::<Vec<_>>();
    crate::package_helpers::connected_components(&adjacency, seeds)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct FunctionAxisToken {
    param_count: u32,
    statement_count: u32,
    kind: AxisKind,
    hash: u64,
}

impl FunctionAxisToken {
    const fn axis_weight(self) -> usize {
        match self.kind {
            AxisKind::Ast => 8,
            AxisKind::Cfg => 5,
            AxisKind::NormalizedCfg => 4,
            AxisKind::LiteralAnchor
            | AxisKind::AccessPattern
            | AxisKind::CalleeSet
            | AxisKind::ThrowSet => 6,
            AxisKind::StructuralAnchor => 2,
            AxisKind::ReturnPattern
            | AxisKind::EffectPattern
            | AxisKind::LiteralShape
            | AxisKind::AccessShape
            | AxisKind::ExpressionShape
            | AxisKind::BindingPattern => 1,
        }
    }
}

fn package_function_axis_index(
    rows: &InputRows,
    package_sources: &[PackageSource],
    include_source_nodes: bool,
) -> FunctionAxisIndex {
    let haystack = rows
        .source_files
        .iter()
        .filter_map(|source_file| source_file.source.as_deref())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join("\n");
    let mut groups = BTreeMap::<(String, String), Vec<&PackageSource>>::new();
    for source in package_sources {
        if source.source.len() > ANONYMOUS_AXIS_SOURCE_MAX_BYTES {
            continue;
        }
        groups
            .entry((source.package_name.clone(), source.package_version.clone()))
            .or_default()
            .push(source);
    }
    let mut ranked_groups = groups.into_iter().collect::<Vec<_>>();
    ranked_groups.sort_by(|left, right| {
        let left_hits = package_name_occurrences(haystack.as_str(), left.0.0.as_str());
        let right_hits = package_name_occurrences(haystack.as_str(), right.0.0.as_str());
        right_hits
            .cmp(&left_hits)
            .then_with(|| left.1.len().cmp(&right.1.len()))
            .then_with(|| left.0.cmp(&right.0))
    });

    let mut token_versions = BTreeMap::<FunctionAxisToken, BTreeSet<(String, String)>>::new();
    let mut token_sources = BTreeMap::<FunctionAxisToken, BTreeSet<PackageSourceNode>>::new();
    let mut processed_sources = 0usize;
    for ((package_name, package_version), sources) in ranked_groups {
        if processed_sources >= ANONYMOUS_AXIS_SOURCE_BUDGET {
            break;
        }
        if processed_sources + sources.len() > ANONYMOUS_AXIS_SOURCE_BUDGET {
            continue;
        }
        processed_sources += sources.len();
        for source in sources {
            let source_node = include_source_nodes.then(|| PackageSourceNode {
                package_name: package_name.clone(),
                package_version: package_version.clone(),
                source_path: source.source_path.clone(),
            });
            for fingerprint in FunctionExtractor::fingerprint(ModuleId(0), source.source.as_str()) {
                for token in function_axis_tokens(&fingerprint) {
                    token_versions
                        .entry(token)
                        .or_default()
                        .insert((package_name.clone(), package_version.clone()));
                    if let Some(source_node) = &source_node {
                        token_sources
                            .entry(token)
                            .or_default()
                            .insert(source_node.clone());
                    }
                }
            }
        }
    }
    FunctionAxisIndex {
        token_versions,
        token_sources,
    }
}

fn best_axis_source_candidate<'a>(
    tokens: &BTreeSet<FunctionAxisToken>,
    axis_index: &'a FunctionAxisIndex,
    package_name: &str,
    package_version: &str,
) -> Option<(&'a PackageSourceNode, usize, usize)> {
    let mut scores = BTreeMap::<&PackageSourceNode, usize>::new();
    for token in tokens {
        let Some(sources) = axis_index.token_sources.get(token) else {
            continue;
        };
        if sources.len() > ANONYMOUS_AXIS_TOKEN_SOURCE_LIMIT {
            continue;
        }
        let weight = token.axis_weight();
        for source in sources {
            if source.package_name == package_name && source.package_version == package_version {
                *scores.entry(source).or_default() += weight;
            }
        }
    }
    let mut ranked = scores.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    let (source, score) = ranked.first().copied()?;
    let runner_up = ranked
        .get(1)
        .map_or(0, |(_source, runner_up_score)| *runner_up_score);
    if score < ANONYMOUS_AXIS_MIN_SCORE
        || score.saturating_sub(runner_up) < ANONYMOUS_AXIS_MIN_MARGIN
    {
        return None;
    }
    Some((source, score, runner_up))
}

fn package_name_occurrences(haystack: &str, package_name: &str) -> usize {
    let needle = package_name.to_ascii_lowercase();
    if needle.len() < 3 {
        return 0;
    }
    haystack.matches(needle.as_str()).count()
}

fn function_axis_tokens(fingerprint: &FunctionFingerprint) -> BTreeSet<FunctionAxisToken> {
    let mut tokens = BTreeSet::new();
    push_axis_tokens(
        fingerprint.param_count,
        fingerprint.statement_count,
        &fingerprint.primary,
        &mut tokens,
    );
    for alternate in &fingerprint.alternates {
        push_axis_tokens(
            fingerprint.param_count,
            alternate.statement_count,
            &alternate.axes,
            &mut tokens,
        );
    }
    tokens
}

fn push_axis_tokens(
    param_count: u32,
    statement_count: u32,
    axes: &reverts_ir::AxisHashes,
    out: &mut BTreeSet<FunctionAxisToken>,
) {
    for (kind, hash) in [
        (AxisKind::Ast, Some(axes.ast)),
        (AxisKind::Cfg, Some(axes.cfg)),
        (AxisKind::NormalizedCfg, Some(axes.normalized_cfg)),
        (AxisKind::StructuralAnchor, Some(axes.structural_anchor)),
        (AxisKind::LiteralAnchor, axes.literal_anchor),
        (AxisKind::AccessPattern, axes.access_pattern),
        (AxisKind::CalleeSet, axes.callee_set),
        (AxisKind::ThrowSet, axes.throw_set),
        (AxisKind::ExpressionShape, axes.expression_shape),
    ] {
        if let Some(hash) = hash {
            out.insert(FunctionAxisToken {
                param_count,
                statement_count,
                kind,
                hash,
            });
        }
    }
}

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
            !context.package_sources.is_empty(),
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
        state.function_attributions =
            externally_attributable_function_matches(context.rows, cascade_report.attributions);
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

struct AnonymousImportablePass;

impl PackageMatchPass for AnonymousImportablePass {
    fn name(&self) -> &'static str {
        "anonymous_importable_promote"
    }

    fn run(&self, context: &PackageMatchContext<'_>, state: &mut PackageMatchState) {
        ownership::importable::promote_anonymous_bundle_external_imports(
            context.rows,
            context.package_sources,
            &mut state.package_report,
        );
    }
}

struct ProvenExternalImportTargetsPass;

impl PackageMatchPass for ProvenExternalImportTargetsPass {
    fn name(&self) -> &'static str {
        "proven_external_import_targets"
    }

    fn run(&self, context: &PackageMatchContext<'_>, state: &mut PackageMatchState) {
        let matched_package_names = context
            .package_filter
            .cloned()
            .unwrap_or_else(|| unmatched_package_scope(context.rows));
        ownership::force_externalize::promote_proven_external_import_targets(
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
        // Ownership / proven-external-import passes can append accepted
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
    allow_anonymous_bundle_modules: bool,
) -> BTreeMap<ModuleId, Vec<FunctionFingerprint>> {
    let mut out = BTreeMap::new();
    for module in &rows.modules {
        if excluded_modules.contains(&module.id) {
            continue;
        }
        let package_hint = module_package_hint(module);
        let anonymous_bundle_candidate =
            allow_anonymous_bundle_modules && is_anonymous_bundle_package_candidate(rows, module);
        if package_hint.is_none() && !anonymous_bundle_candidate {
            continue;
        }
        if let Some(package_filter) = package_filter
            && let Some((package_name, _package_version)) = package_hint
            && !package_filter.contains(package_name)
        {
            continue;
        }
        if let Some(slice) = rows.module_source_slice(module.id) {
            let quality =
                package_module_source_quality(module, slice.source_file_path, slice.source);
            if quality == PackageModuleSourceQuality::Invalid
                || (package_hint.is_some()
                    && only_weak_package_sources
                    && quality != PackageModuleSourceQuality::Weak)
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

fn module_package_hint(module: &reverts_input::ModuleInput) -> Option<(&str, Option<&str>)> {
    if module.kind != ModuleKind::Package {
        return None;
    }
    let package_name = module
        .package_name
        .as_deref()
        .map(str::trim)
        .filter(|package_name| !package_name.is_empty())?;
    let package_version = module
        .package_version
        .as_deref()
        .map(str::trim)
        .filter(|package_version| !package_version.is_empty());
    Some((package_name, package_version))
}

pub(crate) fn is_anonymous_bundle_package_candidate(
    rows: &InputRows,
    module: &reverts_input::ModuleInput,
) -> bool {
    module.kind == ModuleKind::Application
        && module
            .package_name
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
        && !has_accepted_external_attribution(rows, module.id)
}

fn externally_attributable_function_matches(
    rows: &InputRows,
    attributions: Vec<PackageAttributionInput>,
) -> Vec<PackageAttributionInput> {
    let modules_by_id = rows
        .modules
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    attributions
        .into_iter()
        .filter(|attribution| {
            modules_by_id
                .get(&attribution.module_id)
                .and_then(|module| module_package_hint(module))
                .is_some()
        })
        .collect()
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
            let (package_name, package_version_hint) = module_package_hint(module)?;
            let package_version = module
                .package_version
                .as_deref()
                .or(package_version_hint)
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
        if package_name.is_none() {
            let candidate_versions =
                anonymous_candidate_package_versions(rows, &scoped_fingerprints, package_sources);
            if candidate_versions.is_empty() {
                continue;
            }
            let mut sources_by_version = BTreeMap::<(String, String), Vec<PackageSource>>::new();
            for source in package_sources {
                let key = (source.package_name.clone(), source.package_version.clone());
                if !candidate_versions.contains(&key) {
                    continue;
                }
                sources_by_version
                    .entry(key)
                    .or_default()
                    .push(source.clone());
            }
            for ((_source_package_name, _source_package_version), scoped_sources) in
                sources_by_version
            {
                if scoped_sources.len() > CASCADE_SOURCE_GROUP_LIMIT {
                    continue;
                }
                let report = match_with_cascade(&scoped_fingerprints, &scoped_sources);
                merged.attributions.extend(report.attributions);
                merged.ownership_matches.extend(report.ownership_matches);
                merged.audit.extend(report.audit);
            }
            continue;
        }
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

fn anonymous_candidate_package_versions(
    rows: &InputRows,
    scoped_fingerprints: &BTreeMap<ModuleId, Vec<FunctionFingerprint>>,
    package_sources: &[PackageSource],
) -> BTreeSet<(String, String)> {
    let mut module_function_hashes = BTreeSet::new();
    let mut module_string_anchors = BTreeSet::new();
    for module_id in scoped_fingerprints.keys() {
        let Some(module) = rows.modules.iter().find(|module| module.id == *module_id) else {
            continue;
        };
        let Some(slice) = rows.module_source_slice(*module_id) else {
            continue;
        };
        let Ok(fingerprint) =
            module_match_fingerprint(module, slice.source_file_path, slice.source)
        else {
            continue;
        };
        module_function_hashes.extend(fingerprint.function_signature_hashes);
        module_string_anchors.extend(fingerprint.string_anchors);
    }
    if module_function_hashes.is_empty() && module_string_anchors.is_empty() {
        return BTreeSet::new();
    }

    let mut scores = BTreeMap::<(String, String), (usize, usize)>::new();
    for source in package_sources {
        let Ok(fingerprint) = package_source_fingerprint(source) else {
            continue;
        };
        let function_overlap = fingerprint
            .function_signature_hashes
            .intersection(&module_function_hashes)
            .count();
        let string_overlap = fingerprint
            .string_anchors
            .intersection(&module_string_anchors)
            .count();
        if function_overlap < ANONYMOUS_CASCADE_MIN_FUNCTION_SIGNATURES
            && string_overlap < ANONYMOUS_CASCADE_MIN_STRING_ANCHORS
        {
            continue;
        }
        let entry = scores
            .entry((source.package_name.clone(), source.package_version.clone()))
            .or_default();
        entry.0 += function_overlap;
        entry.1 += string_overlap;
    }

    let mut ranked = scores
        .into_iter()
        .map(|(key, (function_overlap, string_overlap))| {
            let score = function_overlap * 20 + string_overlap;
            (key, score, function_overlap, string_overlap)
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| right.2.cmp(&left.2))
            .then_with(|| right.3.cmp(&left.3))
            .then_with(|| left.0.cmp(&right.0))
    });
    ranked
        .into_iter()
        .take(ANONYMOUS_CASCADE_PACKAGE_VERSION_LIMIT)
        .map(|(key, _score, _function_overlap, _string_overlap)| key)
        .collect()
}
