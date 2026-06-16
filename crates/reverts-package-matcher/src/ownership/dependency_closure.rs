//! Module-dependency-closure analysis used by the dependency-closure and
//! dependency-cluster ownership strategies. Computes connected components
//! over package-module dependencies and the directional neighborhood
//! evidence that proves a module is dominated by one package version.

use std::collections::{BTreeMap, BTreeSet};

use reverts_input::{InputRows, ModuleDependencyTarget, ModuleInput};
use reverts_ir::{ModuleId, ModuleKind};

use crate::package_helpers::{direct_module_dependencies, direct_module_dependents};

#[must_use]
pub(crate) fn package_dependency_components(rows: &InputRows) -> Vec<BTreeSet<ModuleId>> {
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

pub(crate) fn has_direct_neighborhood_package_contradiction(
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
pub(crate) struct DependencyNeighborhoodEvidence {
    pub(crate) package_version: String,
    pub(crate) same_package_owned_neighbors: usize,
    pub(crate) owned_neighbors: usize,
    pub(crate) same_version_owned_neighbors: usize,
    pub(crate) same_outgoing_neighbors: usize,
    pub(crate) owned_outgoing_neighbors: usize,
    pub(crate) same_incoming_neighbors: usize,
    pub(crate) owned_incoming_neighbors: usize,
}

pub(crate) fn dependency_neighborhood_ownership_evidence(
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

#[must_use]
pub(crate) fn dependency_neighborhood_source_path(
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
