use std::path::Path;

use oxc_allocator::Allocator;
use oxc_codegen::{CodeGenerator, CodegenOptions};
use oxc_parser::Parser;

use crate::errors::{JsError, ParseError, ParseGoal, Result};
use crate::parse::{parse_options_for, source_type_candidates};

pub fn format_source_pretty(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<String> {
    format_source_with_minify(source, path_hint, goal, false)
}

pub fn format_source_minified(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<String> {
    format_source_with_minify(source, path_hint, goal, true)
}

fn format_source_with_minify(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
    minify: bool,
) -> Result<String> {
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }

        let output = CodeGenerator::new()
            .with_options(CodegenOptions {
                single_quote: true,
                minify,
                ..Default::default()
            })
            .build(&parsed.program);
        return Ok(output.code);
    }

    Err(JsError::ParseFailed(errors))
}

pub fn normalize_source_for_pipeline(source: &str, path_hint: Option<&Path>) -> Result<String> {
    format_source_pretty(source, path_hint, ParseGoal::TypeScript)
}
