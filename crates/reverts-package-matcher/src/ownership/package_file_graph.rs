//! Package-file-graph ownership promotion.
//!
//! Looks for "runs" of adjacent package modules in the same bundle file
//! that share a package name, then admits the un-owned ones as matches
//! whenever the surrounding owned modules already converge on a single
//! `(package_name, package_version)`. This catches modules that the
//! per-module strategies leave unattributed because their individual
//! source slice is too small or too weak to fingerprint.

use std::collections::{BTreeMap, BTreeSet};

use reverts_input::{InputRows, ModuleInput};
use reverts_ir::{ModuleId, ModuleKind};

use crate::{
    ModuleMatchStrategy, PackageMatch, PackageModuleSourceQuality, VersionedPackageMatchReport,
    accepted_external_modules, has_direct_neighborhood_package_contradiction, ownership_by_module,
    package_module_source_quality,
};

pub(crate) fn promote_package_file_graph_ownership_matches(
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
