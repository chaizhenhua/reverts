use std::path::Path;

use reverts_js::{JsError, ParseGoal, parse_source};
use reverts_observe::{AuditFinding, AuditReport, FindingCode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmittedFile {
    pub path: String,
    pub source: String,
}

impl EmittedFile {
    #[must_use]
    pub fn new(path: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            source: source.into(),
        }
    }
}

#[must_use]
pub fn audit_emitted_files_parse(files: &[EmittedFile]) -> AuditReport {
    let mut report = AuditReport::default();
    for file in files {
        if let Err(error) = parse_source(
            &file.source,
            Some(Path::new(file.path.as_str())),
            ParseGoal::TypeScript,
        ) {
            report.push(
                AuditFinding::error(FindingCode::UnparseableOutput, parse_error_message(&error))
                    .with_module(file.path.clone()),
            );
        }
    }
    report
}

fn parse_error_message(error: &JsError) -> String {
    match error {
        JsError::ParseFailed(errors) => errors.first().map_or_else(
            || "output could not be parsed".to_string(),
            |error| {
                let diagnostic = error
                    .diagnostics
                    .first()
                    .map_or("no diagnostic", String::as_str);
                format!(
                    "output could not be parsed as {}: {diagnostic}",
                    error.source_type
                )
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use reverts_observe::FindingCode;

    use super::{EmittedFile, audit_emitted_files_parse};

    #[test]
    fn parseable_emitted_typescript_is_clean() {
        let files = [EmittedFile::new(
            "src/index.ts",
            "const answer: number = 42;",
        )];

        let report = audit_emitted_files_parse(&files);

        assert!(report.is_clean());
    }

    #[test]
    fn unparseable_emitted_typescript_is_reported_without_external_tools() {
        let files = [EmittedFile::new("src/index.ts", "const =")];

        let report = audit_emitted_files_parse(&files);

        assert!(report.has(FindingCode::UnparseableOutput));
        assert_eq!(report.findings()[0].module.as_deref(), Some("src/index.ts"));
    }
}
