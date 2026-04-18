use std::path::Path;

use oxc_allocator::Allocator;
use oxc_codegen::{CodeGenerator, CodegenOptions};
use oxc_parser::Parser;
use oxc_span::SourceType;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub source_type: String,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsError {
    ParseFailed(Vec<ParseError>),
}

pub type Result<T> = std::result::Result<T, JsError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseGoal {
    JavaScript,
    TypeScript,
}

#[must_use]
pub fn source_type_candidates(path_hint: Option<&Path>, goal: ParseGoal) -> Vec<SourceType> {
    let mut candidates = Vec::new();
    if let Some(path_hint) = path_hint
        && let Ok(source_type) = SourceType::from_path(path_hint)
    {
        push_unique(&mut candidates, source_type);
    }

    match goal {
        ParseGoal::JavaScript => {
            push_unique(&mut candidates, SourceType::mjs());
            push_unique(&mut candidates, SourceType::cjs());
            push_unique(&mut candidates, SourceType::jsx());
        }
        ParseGoal::TypeScript => {
            push_unique(&mut candidates, SourceType::tsx());
            push_unique(&mut candidates, SourceType::ts());
            push_unique(&mut candidates, SourceType::mjs().with_typescript(true));
            push_unique(&mut candidates, SourceType::mjs());
            push_unique(&mut candidates, SourceType::jsx());
        }
    }

    candidates
}

fn push_unique(candidates: &mut Vec<SourceType>, source_type: SourceType) {
    if !candidates.contains(&source_type) {
        candidates.push(source_type);
    }
}

pub fn parse_source(source: &str, path_hint: Option<&Path>, goal: ParseGoal) -> Result<()> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type).parse();
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

pub fn format_source_pretty(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<String> {
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, source, source_type).parse();
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
                minify: false,
                ..Default::default()
            })
            .build(&parsed.program);
        return Ok(output.code);
    }

    Err(JsError::ParseFailed(errors))
}

#[must_use]
pub fn sanitize_identifier(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for (index, ch) in value.chars().enumerate() {
        let valid = if index == 0 {
            is_identifier_start(ch) || is_identifier_part(ch)
        } else {
            is_identifier_part(ch)
        };
        output.push(if valid { ch } else { '_' });
    }

    if output.is_empty() {
        return "_".to_string();
    }

    if output
        .chars()
        .next()
        .is_some_and(|first| !is_identifier_start(first))
    {
        output.insert(0, '_');
    }

    if is_reserved_word(&output) {
        output.insert(0, '_');
    }

    output
}

#[must_use]
pub fn is_identifier_start(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphabetic()
}

#[must_use]
pub fn is_identifier_part(ch: char) -> bool {
    is_identifier_start(ch) || ch.is_ascii_digit()
}

fn is_reserved_word(value: &str) -> bool {
    matches!(
        value,
        "await"
            | "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "debugger"
            | "default"
            | "delete"
            | "do"
            | "else"
            | "enum"
            | "export"
            | "extends"
            | "false"
            | "finally"
            | "for"
            | "function"
            | "if"
            | "import"
            | "in"
            | "instanceof"
            | "new"
            | "null"
            | "return"
            | "super"
            | "switch"
            | "this"
            | "throw"
            | "true"
            | "try"
            | "typeof"
            | "var"
            | "void"
            | "while"
            | "with"
            | "yield"
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{JsError, ParseGoal, format_source_pretty, parse_source, sanitize_identifier};

    #[test]
    fn parses_typescript_without_external_tooling() {
        let source = "const answer: number = 42;";

        assert!(parse_source(source, Some(Path::new("fixture.ts")), ParseGoal::TypeScript).is_ok());
    }

    #[test]
    fn reports_parse_failure_without_panicking() {
        let error = parse_source("const =", None, ParseGoal::TypeScript);

        assert!(matches!(error, Err(JsError::ParseFailed(errors)) if !errors.is_empty()));
    }

    #[test]
    fn formats_typescript_through_oxc_codegen() {
        let formatted = format_source_pretty("const x:number=1", None, ParseGoal::TypeScript)
            .expect("fixture should parse");

        assert!(formatted.contains("const x: number = 1"));
    }

    #[test]
    fn sanitizes_package_and_minifier_fragments_into_identifiers() {
        assert_eq!(sanitize_identifier("@smithy/XY7"), "_smithy_XY7");
        assert_eq!(sanitize_identifier("9patch-name"), "_9patch_name");
        assert_eq!(sanitize_identifier("class"), "_class");
    }
}
