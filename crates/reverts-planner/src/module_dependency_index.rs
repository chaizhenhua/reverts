//! Cross-module dependency indexes extracted from `lib.rs`.
//!
//! Builds direct and transitive `ModuleId -> {ModuleId}` reachability maps
//! over the `ModuleDependency` list, and the "source-suppressed package"
//! closure that records which package modules have lost every
//! non-source-preserved consumer. The package-ownership predicate
//! (`package_attribution_proves_package_ownership`) lives here too because
//! the closure consults it to admit packages to the seed set.

use std::collections::{BTreeMap, BTreeSet};

use reverts_input::{
    ModuleDependencyTarget, ModuleInput, PackageAttributionInput, PackageAttributionStatus,
    PackageEmissionMode,
};
use reverts_ir::{ModuleId, ModuleKind};
use reverts_model::EnrichedProgram;

pub(crate) fn module_dependency_path_exists(
    dependencies: &BTreeMap<ModuleId, BTreeSet<ModuleId>>,
    from: ModuleId,
    target: ModuleId,
) -> bool {
    from == target
        || dependencies
            .get(&from)
            .is_some_and(|reachable| reachable.contains(&target))
}

pub(crate) fn module_dependency_modules_by_owner(
    program: &EnrichedProgram,
) -> BTreeMap<ModuleId, BTreeSet<ModuleId>> {
    let mut direct_dependencies = BTreeMap::<ModuleId, BTreeSet<ModuleId>>::new();
    let mut modules = BTreeSet::<ModuleId>::new();
    for dependency in &program.model().input().dependencies {
        let ModuleDependencyTarget::Module(target_module_id) = dependency.target else {
            continue;
        };
        modules.insert(dependency.from_module_id);
        modules.insert(target_module_id);
        direct_dependencies
            .entry(dependency.from_module_id)
            .or_default()
            .insert(target_module_id);
    }
    let mut transitive_dependencies = BTreeMap::<ModuleId, BTreeSet<ModuleId>>::new();
    for module_id in modules {
        let mut reachable = BTreeSet::<ModuleId>::new();
        let mut stack = direct_dependencies
            .get(&module_id)
            .into_iter()
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        while let Some(next) = stack.pop() {
            if !reachable.insert(next) {
                continue;
            }
            if let Some(next_modules) = direct_dependencies.get(&next) {
                stack.extend(next_modules.iter().copied());
            }
        }
        if !reachable.is_empty() {
            transitive_dependencies.insert(module_id, reachable);
        }
    }
    transitive_dependencies
}

pub(crate) fn source_suppressed_package_dependency_closure(
    program: &EnrichedProgram,
    seed_modules: &BTreeSet<ModuleId>,
    source_preserved_packages: &BTreeSet<ModuleId>,
    ownership_proven_packages: &BTreeSet<ModuleId>,
) -> BTreeSet<ModuleId> {
    let modules_by_id = program
        .model()
        .modules()
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let (outgoing_dependencies, incoming_dependencies) = direct_module_dependency_indexes(program);
    let mut reachable = seed_modules
        .iter()
        .copied()
        .filter(|module_id| {
            modules_by_id
                .get(module_id)
                .is_some_and(|module| module.kind == ModuleKind::Package)
                && !source_preserved_packages.contains(module_id)
        })
        .collect::<BTreeSet<_>>();
    let mut stack = reachable.iter().copied().collect::<Vec<_>>();
    while let Some(module_id) = stack.pop() {
        for dependency_id in outgoing_dependencies
            .get(&module_id)
            .into_iter()
            .flatten()
            .copied()
        {
            let Some(dependency) = modules_by_id.get(&dependency_id) else {
                continue;
            };
            if dependency.kind != ModuleKind::Package
                || source_preserved_packages.contains(&dependency_id)
                || !ownership_proven_packages.contains(&dependency_id)
                || !reachable.insert(dependency_id)
            {
                continue;
            }
            stack.push(dependency_id);
        }
    }

    loop {
        let removed = reachable
            .iter()
            .copied()
            .filter(|module_id| !seed_modules.contains(module_id))
            .filter(|module_id| {
                let Some(module) = modules_by_id.get(module_id).copied() else {
                    return false;
                };
                incoming_dependencies
                    .get(module_id)
                    .into_iter()
                    .flatten()
                    .any(|consumer_id| {
                        modules_by_id.get(consumer_id).is_some_and(|consumer| {
                            !reachable.contains(consumer_id)
                                && !source_suppressed_consumer_is_boundary(module, consumer)
                        })
                    })
            })
            .collect::<Vec<_>>();
        if removed.is_empty() {
            break;
        }
        for module_id in removed {
            reachable.remove(&module_id);
        }
    }

    reachable
}

pub(crate) fn source_suppressed_consumer_is_boundary(
    module: &ModuleInput,
    consumer: &ModuleInput,
) -> bool {
    match consumer.kind {
        ModuleKind::Application => true,
        ModuleKind::Package => !source_suppressed_same_package_consumer(module, consumer),
        ModuleKind::Builtin => false,
    }
}

pub(crate) fn source_suppressed_same_package_consumer(
    module: &ModuleInput,
    consumer: &ModuleInput,
) -> bool {
    let Some(module_package) = module.package_name.as_deref().map(str::trim) else {
        return false;
    };
    let Some(consumer_package) = consumer.package_name.as_deref().map(str::trim) else {
        return false;
    };
    !module_package.is_empty() && module_package == consumer_package
}

pub(crate) fn package_ownership_proven_module_ids(program: &EnrichedProgram) -> BTreeSet<ModuleId> {
    let modules_by_id = program
        .model()
        .modules()
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    program
        .model()
        .input()
        .package_attributions
        .iter()
        .filter_map(|attribution| {
            let module = modules_by_id.get(&attribution.module_id).copied()?;
            package_attribution_proves_package_ownership(attribution, module)
                .then_some(attribution.module_id)
        })
        .collect()
}

pub(crate) fn package_attribution_proves_package_ownership(
    attribution: &PackageAttributionInput,
    module: &ModuleInput,
) -> bool {
    if module.kind != ModuleKind::Package
        || module.package_name.as_deref() != Some(attribution.package_name.as_str())
    {
        return false;
    }
    if let Some(attribution_version) = attribution.package_version.as_deref()
        && module
            .package_version
            .as_deref()
            .is_some_and(|module_version| {
                !module_version.trim().is_empty() && module_version != attribution_version
            })
    {
        return false;
    }
    (attribution.status == PackageAttributionStatus::Accepted
        && attribution.emission_mode == PackageEmissionMode::ExternalImport
        && attribution.export_specifier.is_some())
        || (attribution.status == PackageAttributionStatus::Rejected
            && attribution.emission_mode == PackageEmissionMode::ApplicationSource
            && attribution.package_version.is_some())
}

pub(crate) fn direct_module_dependency_indexes(
    program: &EnrichedProgram,
) -> (
    BTreeMap<ModuleId, BTreeSet<ModuleId>>,
    BTreeMap<ModuleId, BTreeSet<ModuleId>>,
) {
    let mut outgoing = BTreeMap::<ModuleId, BTreeSet<ModuleId>>::new();
    let mut incoming = BTreeMap::<ModuleId, BTreeSet<ModuleId>>::new();
    for dependency in &program.model().input().dependencies {
        let ModuleDependencyTarget::Module(target_module_id) = dependency.target else {
            continue;
        };
        outgoing
            .entry(dependency.from_module_id)
            .or_default()
            .insert(target_module_id);
        incoming
            .entry(target_module_id)
            .or_default()
            .insert(dependency.from_module_id);
    }
    (outgoing, incoming)
}
