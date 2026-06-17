//! Delimiter-aware source-surgery primitives used by legacy planner passes.
//!
//! New output behaviour should prefer AST transforms in `reverts-js`. The
//! helpers here are only for cases where the planner must preserve exact raw
//! source snippets (runtime helper bodies, template raw text, or trivia) and
//! OXC cannot round-trip the bytes without changing observable output.

/// Apply non-overlapping byte-range edits to `source`.
///
/// Callers are responsible for producing ranges at syntax-aware boundaries
/// (usually via `byte_lexer` scanners that skip strings, templates, regexes,
/// and comments). This function deliberately only applies already-vetted edits;
/// it does not search source text and therefore is not a repair pass.
pub(crate) fn apply_text_edits(source: &str, edits: &[(usize, usize, String)]) -> String {
    let mut edits = edits.to_vec();
    edits.sort_by_key(|(start, _, _)| *start);
    let mut output = String::with_capacity(source.len());
    let mut cursor = 0usize;
    for (start, end, replacement) in edits {
        debug_assert!(start >= cursor, "text edits must not overlap");
        output.push_str(&source[cursor..start]);
        output.push_str(replacement.as_str());
        cursor = end;
    }
    output.push_str(&source[cursor..]);
    output
}

#[cfg(test)]
mod tests {
    use super::apply_text_edits;

    #[test]
    fn applies_edits_in_source_order_even_when_supplied_unsorted() {
        let source = "alpha beta gamma";
        let edited = apply_text_edits(
            source,
            &[(11, 16, "delta".to_string()), (0, 5, "one".to_string())],
        );
        assert_eq!(edited, "one beta delta");
    }

    #[test]
    fn applies_insertions_without_scanning_source_text() {
        let source = "const value = 1;";
        let edited = apply_text_edits(source, &[(0, 0, "// generated\n".to_string())]);
        assert_eq!(edited, "// generated\nconst value = 1;");
    }

    #[test]
    fn edited_javascript_still_parses_when_ranges_are_statement_safe() {
        let source = "const value = 1;
export { value };";
        let edited = apply_text_edits(
            source,
            &[
                (6, 11, "answer".to_string()),
                (26, 31, "answer".to_string()),
            ],
        );
        reverts_js::parse_source(edited.as_str(), None, reverts_js::ParseGoal::JavaScript)
            .expect("statement-safe source surgery should keep JS parseable");
    }
}
