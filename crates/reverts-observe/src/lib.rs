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
    BundleDetectorAmbiguous,
    BundlerKindUnrecognised,
    UnparseableOutput,
    /// Emitter could not inject planned imports / exports / renames into a
    /// module body (e.g. unparseable `const X;` or JSX comma patterns). The
    /// raw body is emitted unchanged; downstream `audit_emitted_project_parse`
    /// still validates the bytes. We surface this as a Warning so consumers
    /// see which planned synthesis was dropped instead of silently observing a
    /// module that's missing imports.
    EmitterRawBodyPreservedAfterInjectionFailure,
    UnparseablePackageSource,
    AmbiguousPackageMatch,
    IifeClusterDegenerate,
    LowConfidenceAttribution,
    MissingPackageSource,
    AmbiguousPackageSurfaceVersion,
    MissingParseableBody,
    OverlappingFunctionAttribution,
    UnreachableTopLevelCode,
    MissingRequiredAsset,
    /// Paper #7 downstream: a planned `NamespaceObject` binding records
    /// member accesses (e.g. `ns.foo`, `ns.bar`) that the emitted source no
    /// longer mentions. Fires when emit machinery drops or shadows a known
    /// property name, masking a real regression in the namespace surface.
    NamespaceMemberStripped,
    /// Input bundle writes a binding from a member-access chain on an
    /// awaited / called value (e.g. `X = (await fetch(...)).data.client_data`),
    /// then later member-reads `X.foo` without a null guard. The chain can
    /// resolve to `null`/`undefined`, and the unguarded read would crash at
    /// runtime. We surface this as a warning rather than repair (ADR 0002):
    /// the bug exists in the input and would crash the original bundle too;
    /// the decompiler is faithful, not corrective.
    UnprotectedNullableMemberRead,
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
    pub fn warning(code: FindingCode, message: impl Into<String>) -> Self {
        Self {
            code,
            severity: Severity::Warning,
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

    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.findings
            .iter()
            .any(|finding| finding.severity == Severity::Error)
    }

    #[must_use]
    pub fn error_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.severity == Severity::Error)
            .count()
    }

    #[must_use]
    pub fn warning_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.severity == Severity::Warning)
            .count()
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
    fn new_finding_codes_compile_and_are_distinct() {
        let a = FindingCode::OverlappingFunctionAttribution;
        let b = FindingCode::LowConfidenceAttribution;
        assert_ne!(a, b);
    }

    #[test]
    fn audit_finding_warning_constructor_yields_warning_severity() {
        let finding = AuditFinding::warning(FindingCode::LowConfidenceAttribution, "caveat")
            .with_module("m1");
        assert_eq!(finding.severity, Severity::Warning);
        assert_eq!(finding.code, FindingCode::LowConfidenceAttribution);
        assert_eq!(finding.message, "caveat");
        assert_eq!(finding.module.as_deref(), Some("m1"));
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

    #[test]
    fn bundle_extraction_finding_codes_are_distinct() {
        let codes = [
            FindingCode::BundlerKindUnrecognised,
            FindingCode::BundleDetectorAmbiguous,
            FindingCode::MissingParseableBody,
            FindingCode::IifeClusterDegenerate,
        ];
        for (i, a) in codes.iter().enumerate() {
            for (j, b) in codes.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b);
                }
            }
        }
    }
}
