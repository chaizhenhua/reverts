use std::path::Path;

use oxc_allocator::Allocator;
use oxc_parser::{ParseOptions, Parser};
use oxc_span::SourceType;

use crate::errors::{JsError, ParseError, ParseGoal, Result};

#[must_use]
pub fn source_type_candidates(path_hint: Option<&Path>, goal: ParseGoal) -> Vec<SourceType> {
    let primary = source_type_for_parse(path_hint, goal);
    // For TypeScript goal on a non-TS-extension path (e.g. `.formatted.js`
    // files that actually contain TS syntax), the default `tsx` candidate's
    // JSX mode collides with TS generics (`<T>(x)` parses as JSX); fall back
    // to TS-without-JSX so the parser gets a second chance before reporting
    // an extraction failure.
    if matches!(goal, ParseGoal::TypeScript)
        && let Some(path_hint) = path_hint
    {
        let extension = path_hint
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or_default();
        if !matches!(extension, "ts" | "tsx" | "mts" | "cts") {
            return vec![primary, SourceType::ts()];
        }
    }
    vec![primary]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typescript_goal_on_js_extension_falls_back_to_ts_without_jsx() {
        // `.formatted.js` files in the reference cache can contain TS syntax
        // (e.g. `export let X: any;`); `SourceType::tsx()`'s JSX mode collides
        // with TS generics, so we offer `SourceType::ts()` as a second
        // candidate to give the parser a real chance before erroring.
        let path = Path::new("/tmp/foo.formatted.js");
        let candidates = source_type_candidates(Some(path), ParseGoal::TypeScript);
        assert_eq!(candidates.len(), 2, "expected tsx + ts fallback");
        assert!(
            candidates[0].is_typescript() && candidates[0].is_jsx(),
            "first candidate should be TSX"
        );
        assert!(
            candidates[1].is_typescript() && !candidates[1].is_jsx(),
            "second candidate should be TS without JSX"
        );
    }

    #[test]
    fn typescript_goal_on_ts_extension_keeps_single_candidate() {
        // `.ts` paths derive a single source_type from the extension; no
        // fallback needed.
        let path = Path::new("/tmp/foo.ts");
        let candidates = source_type_candidates(Some(path), ParseGoal::TypeScript);
        assert_eq!(candidates.len(), 1);
    }

    #[test]
    fn javascript_goal_unchanged_by_fallback() {
        // The fallback is scoped to ParseGoal::TypeScript; JS goal keeps its
        // single candidate.
        let path = Path::new("/tmp/foo.js");
        let candidates = source_type_candidates(Some(path), ParseGoal::JavaScript);
        assert_eq!(candidates.len(), 1);
    }
}
