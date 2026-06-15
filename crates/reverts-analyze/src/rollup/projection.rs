use crate::rollup::db::{AttributionRow, Snapshot};
use crate::rollup::oracle::{Oracle, OracleVerdict};

#[derive(Debug, Clone)]
pub enum ProjectionKind {
    AlreadyAccepted,
    RolledUp { top_specifier: String },
    StillRejected { reason: String },
    Untouched,
}

#[derive(Debug, Clone)]
pub struct Projection {
    pub module_id: i64,
    pub package_name: String,
    pub package_version: Option<String>,
    pub kind: ProjectionKind,
}

pub fn project(snap: &Snapshot, oracle: &Oracle) -> Vec<Projection> {
    snap.attributions
        .iter()
        .map(|attribution| project_one(attribution, oracle))
        .collect()
}

fn project_one(attribution: &AttributionRow, oracle: &Oracle) -> Projection {
    let kind = if attribution.status == "accepted" && attribution.emission_mode == "external_import"
    {
        ProjectionKind::AlreadyAccepted
    } else if attribution.status == "rejected" && is_closure_ownership_rejection(attribution) {
        match attribution.package_version.as_deref() {
            Some(version) => match oracle.lookup(&attribution.package_name, version) {
                Some(OracleVerdict::Externalizable { top_specifier, .. }) => {
                    ProjectionKind::RolledUp {
                        top_specifier: top_specifier.clone(),
                    }
                }
                Some(OracleVerdict::NotExternalizable { reason }) => {
                    ProjectionKind::StillRejected {
                        reason: reason.clone(),
                    }
                }
                None => ProjectionKind::StillRejected {
                    reason: "package version not in oracle".into(),
                },
            },
            None => ProjectionKind::StillRejected {
                reason: "rejected attribution has no package_version".into(),
            },
        }
    } else {
        ProjectionKind::Untouched
    };

    Projection {
        module_id: attribution.module_id,
        package_name: attribution.package_name.clone(),
        package_version: attribution.package_version.clone(),
        kind,
    }
}

fn is_closure_ownership_rejection(attribution: &AttributionRow) -> bool {
    let Some(json) = &attribution.evidence_json else {
        return false;
    };
    json.contains("\"match_strategy\":\"dependency_closure_ownership\"")
        && json.contains("\"external_importable\":false")
}
