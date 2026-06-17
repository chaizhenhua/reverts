use std::collections::BTreeMap;

use serde::Serialize;

use crate::rollup::model::Snapshot;
use crate::rollup::projection::{Projection, ProjectionKind};

#[derive(Debug, Clone, Serialize, Default)]
pub struct GlobalCounts {
    pub package_modules: usize,
    pub already_accepted: usize,
    pub rolled_up: usize,
    pub still_rejected: usize,
    pub projected_external: usize,
    pub projected_ratio: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PackageCounts {
    pub package_name: String,
    pub package_modules: usize,
    pub already_accepted: usize,
    pub rolled_up: usize,
    pub still_rejected: usize,
    pub projected_external: usize,
    pub projected_ratio: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RollupReport {
    pub global: GlobalCounts,
    pub per_package: Vec<PackageCounts>,
}

pub fn summarize(snap: &Snapshot, projections: &[Projection]) -> RollupReport {
    let package_module_ids: std::collections::BTreeSet<i64> = snap
        .modules
        .iter()
        .filter(|m| m.category == "package")
        .map(|m| m.id)
        .collect();

    let mut per_pkg_modules: BTreeMap<String, usize> = BTreeMap::new();
    for m in &snap.modules {
        if m.category != "package" {
            continue;
        }
        if let Some(name) = &m.package_name {
            *per_pkg_modules.entry(name.clone()).or_default() += 1;
        }
    }

    let mut per_pkg_accepted: BTreeMap<String, usize> = BTreeMap::new();
    let mut per_pkg_rolled: BTreeMap<String, usize> = BTreeMap::new();
    let mut per_pkg_still: BTreeMap<String, usize> = BTreeMap::new();
    let mut global = GlobalCounts {
        package_modules: package_module_ids.len(),
        ..Default::default()
    };

    for proj in projections {
        if !package_module_ids.contains(&proj.module_id) {
            continue;
        }
        match &proj.kind {
            ProjectionKind::AlreadyAccepted => {
                global.already_accepted += 1;
                *per_pkg_accepted
                    .entry(proj.package_name.clone())
                    .or_default() += 1;
            }
            ProjectionKind::RolledUp { .. } => {
                global.rolled_up += 1;
                *per_pkg_rolled.entry(proj.package_name.clone()).or_default() += 1;
            }
            ProjectionKind::StillRejected { .. } => {
                global.still_rejected += 1;
                *per_pkg_still.entry(proj.package_name.clone()).or_default() += 1;
            }
            ProjectionKind::Untouched => {}
        }
    }

    global.projected_external = global.already_accepted + global.rolled_up;
    global.projected_ratio = if global.package_modules == 0 {
        0.0
    } else {
        global.projected_external as f64 / global.package_modules as f64
    };

    let mut per_package: Vec<PackageCounts> = per_pkg_modules
        .into_iter()
        .map(|(name, total)| {
            let acc = per_pkg_accepted.get(&name).copied().unwrap_or(0);
            let roll = per_pkg_rolled.get(&name).copied().unwrap_or(0);
            let still = per_pkg_still.get(&name).copied().unwrap_or(0);
            let projected = acc + roll;
            let ratio = if total == 0 {
                0.0
            } else {
                projected as f64 / total as f64
            };
            PackageCounts {
                package_name: name,
                package_modules: total,
                already_accepted: acc,
                rolled_up: roll,
                still_rejected: still,
                projected_external: projected,
                projected_ratio: ratio,
            }
        })
        .collect();
    per_package.sort_by(|a, b| b.package_modules.cmp(&a.package_modules));
    RollupReport {
        global,
        per_package,
    }
}
