use std::error::Error;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub source_type: String,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsError {
    ParseFailed(Vec<ParseError>),
}

impl fmt::Display for JsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ParseFailed(errors) => {
                let Some(error) = errors.first() else {
                    return write!(formatter, "parse failed");
                };
                let Some(diagnostic) = error.diagnostics.first() else {
                    return write!(formatter, "parse failed for {}", error.source_type);
                };
                write!(
                    formatter,
                    "parse failed for {}: {diagnostic}",
                    error.source_type
                )
            }
        }
    }
}

impl Error for JsError {}

pub type Result<T> = std::result::Result<T, JsError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseGoal {
    JavaScript,
    TypeScript,
}

pub fn parse_error_message(error: &JsError, context: &str) -> String {
    match error {
        JsError::ParseFailed(errors) => errors.first().map_or_else(
            || context.to_string(),
            |error| {
                let diagnostic = error
                    .diagnostics
                    .first()
                    .map_or("no diagnostic", String::as_str);
                format!("{context} as {}: {diagnostic}", error.source_type)
            },
        ),
    }
}
