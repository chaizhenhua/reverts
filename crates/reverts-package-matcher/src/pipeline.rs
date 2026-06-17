use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use reverts_graph::FunctionExtractor;
use reverts_input::InputRows;
use reverts_ir::{ModuleId, ModuleKind};
use reverts_observe::AuditReport;

use crate::index::package_module_source_quality;
use crate::model::{PackageMatchingPipelineReport, PackageModuleSourceQuality, PackageSource};
use crate::proof::concrete_source::unmatched_package_scope;
use crate::strategy::{
    self, CascadeMatchReport, StructuralBagMatchReport,
    match_structural_bags_with_excluded_modules, match_with_cascade,
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
    strategy::structural_bag::promote_structural_bag_ownership_matches(
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
