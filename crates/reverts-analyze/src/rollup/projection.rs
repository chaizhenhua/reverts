use crate::rollup::model::{AttributionRow, Snapshot};
use crate::rollup::oracle::{Oracle, OracleVerdict};

#[derive(Debug, Clone)]
pub enum ProjectionKind {
    AlreadyAccepted,
    RolledUp {
        top_specifier: String,
    },
    /// An accepted external-import row that a previous rollup-apply flipped from
    /// a matcher rejection (its `evidence_json` still records that rejection),
    /// but the current oracle no longer judges the package externalizable. The
    /// stale flip must be reverted to rejected — `apply` is otherwise
    /// forward-only, so a later oracle fix never undoes an earlier over-promotion.
    Revoke {
        reason: String,
    },
    StillRejected {
        reason: String,
    },
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
        // An accepted external-import row whose evidence still records a matcher
        // closure-ownership rejection was flipped by an earlier rollup-apply.
        // Re-validate it against the current oracle: if the package is no longer
        // externalizable (e.g. its top-level hint enumerates no public members),
        // the flip is stale and must be revoked. Rows the matcher accepted as
        // direct external imports (evidence: external_importable=true) never match
        // this and are left as AlreadyAccepted.
        if is_closure_ownership_rejection(attribution) {
            match attribution.package_version.as_deref() {
                Some(version) => match oracle.lookup(&attribution.package_name, version) {
                    Some(OracleVerdict::Externalizable { .. }) => ProjectionKind::AlreadyAccepted,
                    Some(OracleVerdict::NotExternalizable { reason }) => ProjectionKind::Revoke {
                        reason: format!("rollup revoked: {reason}"),
                    },
                    None => ProjectionKind::AlreadyAccepted,
                },
                None => ProjectionKind::AlreadyAccepted,
            }
        } else {
            ProjectionKind::AlreadyAccepted
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rollup::model::{AttributionRow, HintRow, ModuleRow, Snapshot};
    use crate::rollup::oracle::{OracleConfig, build_oracle};

    // An accepted external-import row carrying the matcher's closure-ownership
    // rejection in its evidence (i.e. a row a prior rollup-apply flipped).
    fn flipped_rollup(module_id: i64, name: &str, version: &str) -> AttributionRow {
        AttributionRow {
            module_id,
            package_name: name.to_string(),
            package_version: Some(version.to_string()),
            export_specifier: Some(name.to_string()),
            emission_mode: "external_import".to_string(),
            status: "accepted".to_string(),
            evidence_json: Some(
                "{\"match_strategy\":\"dependency_closure_ownership\",\"external_importable\":false}"
                    .to_string(),
            ),
            rejection_reason: None,
        }
    }

    fn module(id: i64, name: &str, version: &str) -> ModuleRow {
        ModuleRow {
            id,
            category: "package".to_string(),
            package_name: Some(name.to_string()),
            package_version: Some(version.to_string()),
        }
    }

    fn anchor(module_id: i64, name: &str, version: &str) -> AttributionRow {
        AttributionRow {
            module_id,
            package_name: name.to_string(),
            package_version: Some(version.to_string()),
            export_specifier: Some(format!("{name}/entry.js")),
            emission_mode: "external_import".to_string(),
            status: "accepted".to_string(),
            evidence_json: Some(
                "{\"external_import_proof\":\"matched_package_source\"}".to_string(),
            ),
            rejection_reason: None,
        }
    }

    fn hint(name: &str, version: &str, members: &[&str]) -> HintRow {
        HintRow {
            package_name: name.to_string(),
            package_version: version.to_string(),
            export_specifier: name.to_string(),
            public_members: members.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn revokes_stale_rollup_when_package_no_longer_externalizable() {
        // `pkg` has a top-level hint but ZERO public members — the current oracle
        // judges it NotExternalizable (the planner could not rewrite internal
        // reads to a public surface). A private internal that a prior apply
        // flipped to `accepted/external_import → pkg` must be REVOKED, not left
        // stranded as an incoherent root import.
        let snapshot = Snapshot {
            modules: vec![module(1, "pkg", "1.0.0"), module(2, "pkg", "1.0.0")],
            attributions: vec![anchor(1, "pkg", "1.0.0"), flipped_rollup(2, "pkg", "1.0.0")],
            hints: vec![hint("pkg", "1.0.0", &[])],
        };
        let oracle = build_oracle(&snapshot, OracleConfig::default());
        let kinds = project(&snapshot, &oracle);
        let internal = kinds
            .iter()
            .find(|p| p.module_id == 2)
            .expect("internal projected");
        assert!(
            matches!(internal.kind, ProjectionKind::Revoke { .. }),
            "stale rollup of a non-externalizable package must be revoked, got {:?}",
            internal.kind,
        );
    }

    #[test]
    fn keeps_rollup_when_package_still_externalizable() {
        // Same flipped-rollup shape, but `pkg` enumerates public members, so the
        // current oracle still judges it externalizable. The flip is coherent
        // (rollup-everything + planner reconciliation) and must NOT be revoked.
        let snapshot = Snapshot {
            modules: vec![module(1, "pkg", "1.0.0"), module(2, "pkg", "1.0.0")],
            attributions: vec![anchor(1, "pkg", "1.0.0"), flipped_rollup(2, "pkg", "1.0.0")],
            hints: vec![hint("pkg", "1.0.0", &["foo", "bar"])],
        };
        let oracle = build_oracle(&snapshot, OracleConfig::default());
        let kinds = project(&snapshot, &oracle);
        let internal = kinds
            .iter()
            .find(|p| p.module_id == 2)
            .expect("internal projected");
        assert!(
            matches!(internal.kind, ProjectionKind::AlreadyAccepted),
            "coherent rollup of an externalizable package must be kept, got {:?}",
            internal.kind,
        );
    }
}
