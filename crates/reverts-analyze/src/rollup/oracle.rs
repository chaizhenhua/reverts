use std::collections::BTreeMap;

use crate::rollup::model::{AttributionRow, HintRow, ModuleRow, Snapshot};

#[derive(Debug, Clone, Copy)]
pub struct OracleConfig {
    pub direct_match_floor: f64,
}

impl Default for OracleConfig {
    fn default() -> Self {
        Self {
            direct_match_floor: 0.0,
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
            if hint.public_members.is_empty() {
                // The hint exists (someone matched the package surface) but
                // its `public_members_json` was empty when persisted. The
                // planner cannot rewrite bundle-internal binding reads to a
                // public-surface name without knowing what names exist on
                // the surface, so it will keep these modules as adapters and
                // never actually emit `import {…} from '<package>'`. Reporting
                // them as Externalizable inflates the DB metric without
                // changing emit behavior — be honest about the gap.
                OracleVerdict::NotExternalizable {
                    reason: "top-level hint has no public_members enumeration; \
                             planner would still adapter-wrap"
                        .into(),
                }
            } else {
                OracleVerdict::Externalizable {
                    top_specifier: hint.top_specifier.clone(),
                    public_members: hint.public_members.clone(),
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rollup::model::{AttributionRow, HintRow, ModuleRow};

    fn package_module(id: i64, name: &str, version: &str) -> ModuleRow {
        ModuleRow {
            id,
            category: "package".to_string(),
            package_name: Some(name.to_string()),
            package_version: Some(version.to_string()),
        }
    }

    fn accepted_external(module_id: i64, name: &str, version: &str) -> AttributionRow {
        AttributionRow {
            module_id,
            package_name: name.to_string(),
            package_version: Some(version.to_string()),
            export_specifier: Some(name.to_string()),
            emission_mode: "external_import".to_string(),
            status: "accepted".to_string(),
            evidence_json: Some(
                "{\"external_import_proof\":\"matched_package_source\"}".to_string(),
            ),
            rejection_reason: None,
        }
    }

    fn top_hint(name: &str, version: &str, members: &[&str]) -> HintRow {
        HintRow {
            package_name: name.to_string(),
            package_version: version.to_string(),
            export_specifier: name.to_string(),
            public_members: members.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn oracle_rejects_externalization_when_top_level_hint_has_no_public_members() {
        // The hint table can be populated by `package-externalization-hints`
        // without enumerating public surface members — for example when the
        // matcher finds the package by source-cache alone. Reporting these
        // verdicts as Externalizable lies about what the planner can emit.
        let snapshot = Snapshot {
            modules: vec![package_module(1, "lodash", "4.17.21")],
            attributions: vec![accepted_external(1, "lodash", "4.17.21")],
            hints: vec![top_hint("lodash", "4.17.21", &[])],
        };
        let oracle = build_oracle(&snapshot, OracleConfig::default());
        match oracle.lookup("lodash", "4.17.21") {
            Some(OracleVerdict::NotExternalizable { reason }) => {
                assert!(
                    reason.contains("public_members"),
                    "rejection reason must explain the empty-members gap: {reason}",
                );
            }
            other => panic!("expected NotExternalizable, got {other:?}"),
        }
    }

    #[test]
    fn oracle_accepts_externalization_when_hint_enumerates_public_members() {
        let snapshot = Snapshot {
            modules: vec![package_module(1, "lodash", "4.17.21")],
            attributions: vec![accepted_external(1, "lodash", "4.17.21")],
            hints: vec![top_hint("lodash", "4.17.21", &["merge", "cloneDeep"])],
        };
        let oracle = build_oracle(&snapshot, OracleConfig::default());
        match oracle.lookup("lodash", "4.17.21") {
            Some(OracleVerdict::Externalizable {
                top_specifier,
                public_members,
            }) => {
                assert_eq!(top_specifier, "lodash");
                assert_eq!(
                    public_members,
                    &vec!["merge".to_string(), "cloneDeep".to_string()],
                );
            }
            other => panic!("expected Externalizable, got {other:?}"),
        }
    }
}
