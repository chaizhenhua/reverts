use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingCode {
    MissingDefinition,
    UnresolvableBareImport,
    CallableEmittedAsNonCallable,
    DuplicateTopLevelBinding,
    SyntheticReferenceWithoutDeclaration,
    UnparseableOutput,
    UnparseablePackageSource,
    AmbiguousPackageMatch,
    MissingPackageSource,
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
