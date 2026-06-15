use std::collections::BTreeMap;

use crate::rollup::db::{AttributionRow, HintRow, ModuleRow, Snapshot};

#[derive(Debug, Clone, Copy)]
pub struct OracleConfig {
    pub direct_match_floor: f64,
}

impl Default for OracleConfig {
    fn default() -> Self {
        Self {
            direct_match_floor: 0.30,
        }
    }
}

#[derive(Debug, Clone)]
pub enum OracleVerdict {
    Externalizable {
        top_specifier: String,
        public_members: Vec<String>,
    },
    NotExternalizable {
        reason: String,
    },
}

#[derive(Debug, Clone, Default)]
pub struct Oracle {
    table: BTreeMap<(String, String), OracleVerdict>,
}

impl Oracle {
    pub fn lookup(&self, name: &str, version: &str) -> Option<&OracleVerdict> {
        self.table.get(&(name.to_string(), version.to_string()))
    }
    pub fn iter(&self) -> impl Iterator<Item = (&(String, String), &OracleVerdict)> {
        self.table.iter()
    }
}

pub fn build_oracle(snap: &Snapshot, cfg: OracleConfig) -> Oracle {
    let mut by_pkg_version: BTreeMap<(String, String), Vec<&ModuleRow>> = BTreeMap::new();
    for module in &snap.modules {
        if module.category != "package" {
            continue;
        }
        let (Some(name), Some(version)) = (&module.package_name, &module.package_version) else {
            continue;
        };
        by_pkg_version
            .entry((name.clone(), version.clone()))
            .or_default()
            .push(module);
    }

    let mut accepted_direct_by_pkg: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut attribution_count_by_pkg: BTreeMap<(String, String), usize> = BTreeMap::new();
    for attribution in &snap.attributions {
        let Some(version) = &attribution.package_version else {
            continue;
        };
        let key = (attribution.package_name.clone(), version.clone());
        *attribution_count_by_pkg.entry(key.clone()).or_default() += 1;
        if attribution.status == "accepted"
            && attribution.emission_mode == "external_import"
            && is_direct_subpath_proof(attribution)
        {
            *accepted_direct_by_pkg.entry(key).or_default() += 1;
        }
    }

    let hint_index = build_hint_index(&snap.hints);

    let mut table = BTreeMap::new();
    for ((name, version), modules) in by_pkg_version {
        let total = modules.len();
        let accepted = accepted_direct_by_pkg
            .get(&(name.clone(), version.clone()))
            .copied()
            .unwrap_or(0);
        let attributions = attribution_count_by_pkg
            .get(&(name.clone(), version.clone()))
            .copied()
            .unwrap_or(0);
        let key = (name.clone(), version.clone());
        let direct_ratio = if total == 0 {
            0.0
        } else {
            (accepted as f64) / (total as f64)
        };
        let has_anchor = accepted > 0 || attributions > 0;
        let verdict = if !has_anchor {
            OracleVerdict::NotExternalizable {
                reason: "no matcher attribution for this version".into(),
            }
        } else if accepted > 0 && direct_ratio < cfg.direct_match_floor {
            OracleVerdict::NotExternalizable {
                reason: format!(
                    "direct match ratio {accepted}/{total} below floor {floor:.2}",
                    floor = cfg.direct_match_floor
                ),
            }
        } else if let Some(hint) = hint_index.top_level_for(&name, &version) {
            OracleVerdict::Externalizable {
                top_specifier: hint.top_specifier.clone(),
                public_members: hint.public_members.clone(),
            }
        } else {
            OracleVerdict::NotExternalizable {
                reason: "no top-level externalization hint".into(),
            }
        };
        table.insert(key, verdict);
    }

    Oracle { table }
}

fn is_direct_subpath_proof(attribution: &AttributionRow) -> bool {
    let Some(spec) = &attribution.export_specifier else {
        return false;
    };
    if spec.trim().is_empty() {
        return false;
    }
    let Some(json) = &attribution.evidence_json else {
        return false;
    };
    json.contains("\"external_import_proof\":\"matched_package_source\"")
}

struct HintIndex {
    top_level: BTreeMap<(String, String), TopLevel>,
}

#[derive(Clone)]
struct TopLevel {
    top_specifier: String,
    public_members: Vec<String>,
}

impl HintIndex {
    fn top_level_for(&self, name: &str, version: &str) -> Option<&TopLevel> {
        self.top_level.get(&(name.to_string(), version.to_string()))
    }
}

fn build_hint_index(rows: &[HintRow]) -> HintIndex {
    let mut top_level: BTreeMap<(String, String), TopLevel> = BTreeMap::new();
    for row in rows {
        let is_top = row.export_specifier == row.package_name;
        if !is_top {
            continue;
        }
        top_level.insert(
            (row.package_name.clone(), row.package_version.clone()),
            TopLevel {
                top_specifier: row.export_specifier.clone(),
                public_members: row.public_members.clone(),
            },
        );
    }
    HintIndex { top_level }
}
