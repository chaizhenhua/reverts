use std::path::Path;

use oxc_allocator::Allocator;
use oxc_parser::{ParseOptions, Parser};
use oxc_span::SourceType;

use crate::errors::{JsError, ParseError, ParseGoal, Result};

#[must_use]
pub fn source_type_candidates(path_hint: Option<&Path>, goal: ParseGoal) -> Vec<SourceType> {
    vec![source_type_for_parse(path_hint, goal)]
}

#[must_use]
pub fn source_type_for_parse(path_hint: Option<&Path>, goal: ParseGoal) -> SourceType {
    match goal {
        ParseGoal::JavaScript => {
            if let Some(path_hint) = path_hint
                && let Ok(source_type) = SourceType::from_path(path_hint)
            {
                return source_type;
            }
            SourceType::mjs()
        }
        ParseGoal::TypeScript => {
            if let Some(path_hint) = path_hint {
                let extension = path_hint
                    .extension()
                    .and_then(std::ffi::OsStr::to_str)
                    .unwrap_or_default();
                if matches!(extension, "ts" | "tsx" | "mts" | "cts")
                    && let Ok(source_type) = SourceType::from_path(path_hint)
                {
                    return source_type;
                }
            }
            SourceType::tsx()
        }
    }
}

pub fn parse_source(source: &str, path_hint: Option<&Path>, goal: ParseGoal) -> Result<()> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if parsed.errors.is_empty() && !parsed.panicked {
            return Ok(());
        }
        errors.push(ParseError {
            source_type: format!("{source_type:?}"),
            diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
        });
    }

    Err(JsError::ParseFailed(errors))
}

#[must_use]
pub fn parse_options_for(_source_type: SourceType) -> ParseOptions {
    ParseOptions {
        allow_return_outside_function: true,
        ..Default::default()
    }
}
