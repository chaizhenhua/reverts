use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindingCode {
    MissingDefinition,
    UnresolvableBareImport,
    CallableEmittedAsNonCallable,
    DuplicateTopLevelBinding,
    SyntheticReferenceWithoutDeclaration,
    AstFactExtractionFailed,
    AmbiguousBindingShape,
    UnparseableOutput,
    UnparseablePackageSource,
    AmbiguousPackageMatch,
    MissingPackageSource,
    UnreachableTopLevelCode,
    MissingRequiredAsset,
    /// Paper #7 downstream: a planned `NamespaceObject` binding records
    /// member accesses (e.g. `ns.foo`, `ns.bar`) that the emitted source no
    /// longer mentions. Fires when emit machinery drops or shadows a known
    /// property name, masking a real regression in the namespace surface.
    NamespaceMemberStripped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditFinding {
    pub code: FindingCode,
    pub severity: Severity,
    pub module: Option<String>,
    pub binding: Option<String>,
    pub message: String,
}

impl AuditFinding {
    #[must_use]
    pub fn error(code: FindingCode, message: impl Into<String>) -> Self {
        Self {
            code,
            severity: Severity::Error,
            module: None,
            binding: None,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn with_module(mut self, module: impl Into<String>) -> Self {
        self.module = Some(module.into());
        self
    }

    #[must_use]
    pub fn with_binding(mut self, binding: impl Into<String>) -> Self {
        self.binding = Some(binding.into());
        self
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AuditReport {
    findings: Vec<AuditFinding>,
}

impl AuditReport {
    pub fn push(&mut self, finding: AuditFinding) {
        self.findings.push(finding);
    }

    pub fn extend(&mut self, other: Self) {
        self.findings.extend(other.findings);
    }

    #[must_use]
    pub fn findings(&self) -> &[AuditFinding] {
        &self.findings
    }

    #[must_use]
    pub fn has(&self, code: FindingCode) -> bool {
        self.findings.iter().any(|finding| finding.code == code)
    }

    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryEvent {
    pub phase: String,
    pub event: String,
    pub detail: Option<String>,
    pub duration: Option<Duration>,
}

impl TelemetryEvent {
    #[must_use]
    pub fn new(phase: impl Into<String>, event: impl Into<String>) -> Self {
        Self {
            phase: phase.into(),
            event: event.into(),
            detail: None,
            duration: None,
        }
    }

    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{AuditFinding, AuditReport, FindingCode, Severity, TelemetryEvent};

    #[test]
    fn audit_finding_builders_record_context_without_changing_severity() {
        let finding = AuditFinding::error(FindingCode::MissingDefinition, "missing binding")
            .with_module("module-1")
            .with_binding("activate");

        assert_eq!(finding.code, FindingCode::MissingDefinition);
        assert_eq!(finding.severity, Severity::Error);
        assert_eq!(finding.message, "missing binding");
        assert_eq!(finding.module.as_deref(), Some("module-1"));
        assert_eq!(finding.binding.as_deref(), Some("activate"));
    }

    #[test]
    fn audit_report_tracks_clean_state_has_and_extend() {
        let mut report = AuditReport::default();
        assert!(report.is_clean());
        assert!(!report.has(FindingCode::UnparseableOutput));

        report.push(AuditFinding::error(
            FindingCode::UnparseableOutput,
            "output failed to parse",
        ));
        assert!(!report.is_clean());
        assert!(report.has(FindingCode::UnparseableOutput));
        assert_eq!(report.findings().len(), 1);

        let mut other = AuditReport::default();
        other.push(AuditFinding::error(
            FindingCode::UnresolvableBareImport,
            "package surface is unknown",
        ));
        report.extend(other);

        assert_eq!(report.findings().len(), 2);
        assert!(report.has(FindingCode::UnresolvableBareImport));
    }

    #[test]
    fn telemetry_event_records_optional_detail_and_duration() {
        let mut event = TelemetryEvent::new("emit", "parse").with_detail("src/index.ts");
        event.duration = Some(Duration::from_millis(7));

        assert_eq!(event.phase, "emit");
        assert_eq!(event.event, "parse");
        assert_eq!(event.detail.as_deref(), Some("src/index.ts"));
        assert_eq!(event.duration, Some(Duration::from_millis(7)));
    }
}
