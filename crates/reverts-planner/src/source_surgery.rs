//! Delimiter-aware source-surgery primitives used by legacy planner passes.
//!
//! New output behaviour should prefer AST transforms in `reverts-js`. The
//! helpers here are only for cases where the planner must preserve exact raw
//! source snippets (runtime helper bodies, template raw text, or trivia) and
//! OXC cannot round-trip the bytes without changing observable output.

use reverts_js::{
    ParseGoal, collect_top_level_statement_facts, skip_block_comment, skip_line_comment,
};

use crate::byte_lexer::{looks_like_regex_literal, skip_quoted};

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

/// Expand statement-removal edits to consume a neighbouring newline.
///
/// Several passes remove entire top-level statements after a syntax-aware
/// scanner has identified their byte ranges. Keeping the newline policy here
/// prevents each pass from hand-rolling subtly different text surgery.
///
/// Adjacency safety: two consecutive removed statements share the single newline
/// between them. The earlier edit consumes it as its *trailing* newline; without
/// care the later edit would also try to consume it as its *leading* newline,
/// producing overlapping ranges that `apply_text_edits` cannot apply. Edits are
/// therefore sorted and each expanded `drop_start` is clamped to the previous
/// edit's `drop_end`, so the shared newline is removed exactly once.
pub(crate) fn expand_line_removal_edits(
    source: &str,
    edits: &[(usize, usize, String)],
) -> Vec<(usize, usize, String)> {
    let mut sorted = edits.to_vec();
    sorted.sort_by_key(|(start, _, _)| *start);
    let mut previous_drop_end = 0usize;
    let mut expanded = Vec::with_capacity(sorted.len());
    for (start, end, replacement) in sorted {
        let mut drop_start = start;
        let mut drop_end = end;
        if source.as_bytes().get(drop_end) == Some(&b'\n') {
            drop_end += 1;
        } else if drop_start > 0 && source.as_bytes().get(drop_start - 1) == Some(&b'\n') {
            drop_start -= 1;
        }
        // Never overlap the previous edit: the shared newline (if any) was
        // already claimed by it. Clamp into `[previous_drop_end, drop_end]` so
        // the range stays well-formed even for back-to-back removals.
        drop_start = drop_start.max(previous_drop_end).min(drop_end);
        previous_drop_end = drop_end;
        expanded.push((drop_start, drop_end, replacement));
    }
    expanded
}

/// Return parser-derived top-level statement slices for a generated module.
///
/// This remains in source surgery because consumers use the byte ranges to
/// remove or rewrite exact statements while preserving raw snippets/trivia.
pub(crate) fn top_level_statement_slices(source: &str) -> Vec<&str> {
    top_level_statement_spans(source)
        .into_iter()
        .map(|(start, end)| &source[start..end])
        .collect()
}

/// Return parser-derived top-level statement byte ranges for a generated module.
pub(crate) fn top_level_statement_spans(source: &str) -> Vec<(usize, usize)> {
    collect_top_level_statement_facts(source, None, ParseGoal::TypeScript)
        .map(|facts| {
            facts
                .into_iter()
                .map(|fact| (fact.byte_start as usize, fact.byte_end as usize))
                .collect()
        })
        .unwrap_or_default()
}

/// True when an unparenthesized initializer `=` appears after `cursor`.
///
/// This is a delimiter-aware byte scan used after parser-derived statement
/// slicing; it is not used to discover arbitrary JS syntax.
pub(crate) fn contains_top_level_initializer_operator(source: &str, mut cursor: usize) -> bool {
    let bytes = source.as_bytes();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = crate::byte_lexer::skip_regex_literal(bytes, cursor);
            }
            b'(' => {
                paren_depth += 1;
                cursor += 1;
            }
            b'[' => {
                bracket_depth += 1;
                cursor += 1;
            }
            b'{' => {
                brace_depth += 1;
                cursor += 1;
            }
            b')' => {
                paren_depth = paren_depth.saturating_sub(1);
                cursor += 1;
            }
            b']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                cursor += 1;
            }
            b'}' => {
                brace_depth = brace_depth.saturating_sub(1);
                cursor += 1;
            }
            b'=' if paren_depth == 0
                && bracket_depth == 0
                && brace_depth == 0
                && bytes.get(cursor + 1) != Some(&b'=')
                && bytes.get(cursor + 1) != Some(&b'>') =>
            {
                return true;
            }
            _ => cursor += 1,
        }
    }
    false
}

/// Return the previous non-whitespace byte index before `before`.
pub(crate) fn previous_non_ws(bytes: &[u8], before: usize) -> Option<usize> {
    let mut cursor = before.checked_sub(1)?;
    while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
        cursor = cursor.checked_sub(1)?;
    }
    Some(cursor)
}

#[cfg(test)]
mod tests {
    use super::{apply_text_edits, expand_line_removal_edits};

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

    #[test]
    fn line_removal_edits_consume_trailing_newline() {
        let source = "const a = 1;\nconst b = 2;\n";
        let edits = expand_line_removal_edits(source, &[(0, 12, String::new())]);
        assert_eq!(apply_text_edits(source, &edits), "const b = 2;\n");
    }

    #[test]
    fn line_removal_edits_consume_leading_newline_at_eof() {
        let source = "const a = 1;\nconst b = 2;";
        let edits = expand_line_removal_edits(source, &[(13, 25, String::new())]);
        assert_eq!(apply_text_edits(source, &edits), "const a = 1;");
    }

    #[test]
    fn adjacent_line_removals_do_not_overlap_on_shared_newline() {
        // Two back-to-back removals: the first consumes the trailing newline at
        // byte 12; without clamping, the second would consume the same newline
        // as its leading newline, producing overlapping ranges that
        // `apply_text_edits` cannot apply. Removing both statements yields "".
        let source = "const a = 1;\nconst b = 2;";
        let edits =
            expand_line_removal_edits(source, &[(0, 12, String::new()), (13, 25, String::new())]);
        // Sorted, expanded ranges must be non-overlapping (start_n >= end_{n-1}).
        for window in edits.windows(2) {
            assert!(window[1].0 >= window[0].1, "overlapping edits: {edits:?}");
        }
        assert_eq!(apply_text_edits(source, &edits), "");
    }

    #[test]
    fn delimiter_scanner_skips_strings_comments_templates_and_regex_literals() {
        let source = r#"call(")", /* ) */ `template ${")"}`, /[)]/.test(value), actual)"#;
        let close = crate::byte_lexer::find_matching_paren(source, 4)
            .expect("outer call paren should be found");
        assert_eq!(&source[close..=close], ")");
        assert_eq!(&source[close - "actual".len()..close], "actual");
    }
}
