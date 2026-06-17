//! Runtime literal coalescing/compaction passes extracted from `lib.rs`.
//!
//! These passes shrink the textual size of lowered runtime helper bodies:
//! - `coalesce_runtime_lazy_initializer_call_runs` merges consecutive
//!   zero-arg call statements in a `lazyValue(() => { ... })` body into a
//!   single comma-separated expression.
//! - `compact_pure_static_runtime_literals` collapses safe top-level
//!   object/array literals onto a single line.
//! - source edits are applied through `source_surgery::apply_text_edits`;
//!   callers here produce syntax-aware ranges before handing them off.

use crate::apply_text_edits;
use crate::byte_lexer::{
    find_matching_brace, find_matching_bracket, find_matching_paren, looks_like_regex_literal,
    skip_non_code_at, skip_quoted, skip_regex_literal, skip_template_literal, skip_ws,
};
use crate::destructure_writes::split_top_level_properties;
use crate::identifiers::parse_identifier;
use crate::lazy_initializer_parse::try_parse_runtime_lazy_initializer_declaration;
use crate::pure_expression::{
    find_top_level_byte, is_literal_expression, pure_object_property_key,
};
use crate::top_level_statement_spans;
use reverts_js::{
    is_ascii_identifier_continue as is_identifier_continue,
    is_ascii_identifier_start as is_identifier_start, is_js_keyword, skip_block_comment,
    skip_line_comment,
};

use crate::previous_non_ws;

pub(crate) fn coalesce_runtime_lazy_initializer_call_runs(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut edits = Vec::<(usize, usize, String)>::new();
    let mut cursor = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && let Some((initializer, after)) =
                try_parse_runtime_lazy_initializer_declaration(source, cursor)
        {
            edits.extend(coalesced_lazy_body_call_run_edits(
                source,
                initializer.body,
                initializer.body_span,
            ));
            cursor = after;
            continue;
        }
        match bytes[cursor] {
            b'(' => paren_depth += 1,
            b')' => paren_depth = paren_depth.saturating_sub(1),
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            b'{' => brace_depth += 1,
            b'}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
        cursor += 1;
    }
    if edits.is_empty() {
        source.to_string()
    } else {
        apply_text_edits(source, &edits)
    }
}

pub(crate) fn coalesced_lazy_body_call_run_edits(
    full_source: &str,
    body: &str,
    body_span: (usize, usize),
) -> Vec<(usize, usize, String)> {
    let statements = top_level_statement_spans(body);
    let mut edits = Vec::<(usize, usize, String)>::new();
    let mut run = Vec::<(usize, usize, String)>::new();
    for (start, end) in statements {
        let statement = &body[start..end];
        if let Some(expression) = coalescible_lazy_body_expression(statement) {
            run.push((start, end, expression));
            continue;
        }
        flush_lazy_body_call_run_edit(full_source, body_span, &mut run, &mut edits);
    }
    flush_lazy_body_call_run_edit(full_source, body_span, &mut run, &mut edits);
    edits
}

pub(crate) fn flush_lazy_body_call_run_edit(
    full_source: &str,
    body_span: (usize, usize),
    run: &mut Vec<(usize, usize, String)>,
    edits: &mut Vec<(usize, usize, String)>,
) {
    if run.len() < 2 {
        run.clear();
        return;
    }
    let (first_start, _, _) = run.first().expect("run length checked");
    let (_, last_end, _) = run.last().expect("run length checked");
    let absolute_start = body_span.0 + *first_start;
    let absolute_end = body_span.0 + *last_end;
    let first_statement = &full_source[absolute_start..body_span.0 + run[0].1];
    let leading = statement_leading_whitespace(first_statement);
    let expression = run
        .iter()
        .map(|(_, _, expression)| expression.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    edits.push((
        absolute_start,
        absolute_end,
        format!("{leading}{expression};"),
    ));
    run.clear();
}

pub(crate) fn statement_leading_whitespace(statement: &str) -> &str {
    let trimmed_start = statement.trim_start();
    &statement[..statement.len() - trimmed_start.len()]
}

pub(crate) fn coalescible_lazy_body_expression(statement: &str) -> Option<String> {
    let trimmed = statement.trim();
    let expression = trimmed.strip_suffix(';').unwrap_or(trimmed).trim();
    if expression == "void 0" {
        return Some(expression.to_string());
    }
    let (callee, after_callee) = parse_identifier(expression, 0)?;
    if is_js_keyword(callee) {
        return None;
    }
    let bytes = expression.as_bytes();
    let open = skip_ws(bytes, after_callee);
    if bytes.get(open) != Some(&b'(') {
        return None;
    }
    let close = find_matching_paren(expression, open)?;
    if !expression[open + 1..close].trim().is_empty() {
        return None;
    }
    (skip_ws(bytes, close + 1) == expression.len()).then(|| expression.to_string())
}

pub(crate) fn compact_pure_static_runtime_literals(source: &str) -> String {
    let mut edits = Vec::<(usize, usize, String)>::new();
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        if matches!(bytes[cursor], b'{' | b'[')
            && container_literal_can_start_runtime_compaction(source, cursor)
        {
            let literal_end = match bytes[cursor] {
                b'{' => find_matching_brace(source, cursor),
                b'[' => find_matching_bracket(source, cursor),
                _ => None,
            };
            let Some(literal_end) = literal_end else {
                cursor += 1;
                continue;
            };
            let literal = &source[cursor..=literal_end];
            if let Some(replacement) = compact_static_container_literal(literal) {
                edits.push((cursor, literal_end + 1, replacement));
                cursor = literal_end + 1;
                continue;
            }
        }
        cursor += 1;
    }
    if edits.is_empty() {
        source.to_string()
    } else {
        apply_text_edits(source, &edits)
    }
}

pub(crate) fn container_literal_can_start_runtime_compaction(
    source: &str,
    literal_start: usize,
) -> bool {
    let bytes = source.as_bytes();
    let Some(previous) = previous_non_ws(bytes, literal_start) else {
        return false;
    };
    match bytes[previous] {
        b'=' => {
            !matches!(
                previous_non_ws(bytes, previous).map(|index| bytes[index]),
                Some(b'=' | b'!' | b'<' | b'>')
            ) && !matches!(bytes.get(previous + 1), Some(b'=') | Some(b'>'))
        }
        b':' | b'(' | b'[' | b',' | b'?' => true,
        _ => false,
    }
}

pub(crate) fn compact_static_container_literal(literal: &str) -> Option<String> {
    if literal.lines().count() < 6
        || !static_container_literal_is_compaction_safe(literal)
        || !static_literal_is_text_compaction_safe(literal)
    {
        return None;
    }
    let replacement = compact_static_literal_text(literal);
    if replacement.lines().count() < literal.lines().count() && replacement.len() < literal.len() {
        Some(replacement)
    } else {
        None
    }
}

pub(crate) fn static_literal_is_text_compaction_safe(literal: &str) -> bool {
    let blocked_keywords = [
        "await", "break", "case", "catch", "class", "const", "continue", "do", "else", "for",
        "function", "if", "let", "return", "switch", "throw", "try", "var", "while", "yield",
    ];
    let bytes = literal.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(literal, cursor) {
            cursor = next;
            continue;
        }
        if bytes[cursor] == b'=' && bytes.get(cursor + 1) == Some(&b'>') {
            return false;
        }
        if is_identifier_start(bytes[cursor]) {
            let start = cursor;
            cursor += 1;
            while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
                cursor += 1;
            }
            let word = &literal[start..cursor];
            if blocked_keywords.contains(&word) {
                return false;
            }
            continue;
        }
        cursor += 1;
    }
    true
}

pub(crate) fn compact_static_literal_text(literal: &str) -> String {
    let bytes = literal.as_bytes();
    let mut output = String::with_capacity(literal.len());
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' => {
                let next = skip_quoted(bytes, cursor, bytes[cursor]);
                output.push_str(&literal[cursor..next]);
                cursor = next;
            }
            b'`' => {
                let next = skip_template_literal(bytes, cursor);
                output.push_str(&literal[cursor..next]);
                cursor = next;
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                let next = skip_line_comment(bytes, cursor + 2);
                if compact_removed_separator_needs_space(&output, literal, next) {
                    output.push(' ');
                }
                cursor = next;
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                let next = skip_block_comment(bytes, cursor + 2);
                if compact_removed_separator_needs_space(&output, literal, next) {
                    output.push(' ');
                }
                cursor = next;
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                let next = skip_regex_literal(bytes, cursor);
                output.push_str(&literal[cursor..next]);
                cursor = next;
            }
            byte if byte.is_ascii_whitespace() => {
                let next = skip_ws(bytes, cursor);
                if compact_removed_separator_needs_space(&output, literal, next) {
                    output.push(' ');
                }
                cursor = next;
            }
            _ => {
                output.push(bytes[cursor] as char);
                cursor += 1;
            }
        }
    }
    output
}

pub(crate) fn compact_removed_separator_needs_space(
    output: &str,
    source: &str,
    next: usize,
) -> bool {
    let Some(previous) = output.as_bytes().last().copied() else {
        return false;
    };
    let Some(next) = source.as_bytes().get(next).copied() else {
        return false;
    };
    (is_identifier_continue(previous) && is_identifier_continue(next))
        || matches!(
            (previous, next),
            (b'+', b'+') | (b'-', b'-') | (b'/', b'/') | (b'/', b'*')
        )
}

pub(crate) fn static_container_literal_is_compaction_safe(literal: &str) -> bool {
    match literal.as_bytes().first().copied() {
        Some(b'{') => compactable_object_container_literal(literal),
        Some(b'[') => compactable_array_container_literal(literal),
        _ => false,
    }
}

pub(crate) fn compactable_object_container_literal(source: &str) -> bool {
    let Some(close) = find_matching_brace(source, 0) else {
        return false;
    };
    if skip_ws(source.as_bytes(), close + 1) != source.len() {
        return false;
    }
    split_top_level_properties(&source[1..close])
        .into_iter()
        .all(compactable_object_container_property)
}

pub(crate) fn compactable_object_container_property(property: &str) -> bool {
    let property = property.trim();
    if property.is_empty() {
        return false;
    }
    if let Some(spread) = property.strip_prefix("...") {
        return simple_runtime_reference_expression(spread.trim());
    }
    if let Some(colon) = find_top_level_byte(property, b':') {
        if !pure_object_property_key(property[..colon].trim()) {
            return false;
        }
        return compactable_container_value_expression(property[colon + 1..].trim());
    }
    simple_runtime_reference_expression(property)
}

pub(crate) fn compactable_array_container_literal(source: &str) -> bool {
    let Some(close) = find_matching_bracket(source, 0) else {
        return false;
    };
    if skip_ws(source.as_bytes(), close + 1) != source.len() {
        return false;
    }
    split_top_level_properties(&source[1..close])
        .into_iter()
        .all(|element| {
            let element = element.trim();
            if element.is_empty() {
                return false;
            }
            if let Some(spread) = element.strip_prefix("...") {
                return simple_runtime_reference_expression(spread.trim());
            }
            compactable_container_value_expression(element)
        })
}

pub(crate) fn compactable_container_value_expression(source: &str) -> bool {
    let source = source.trim();
    if source.is_empty() {
        return false;
    }
    if matches!(source, "void 0")
        || is_literal_expression(source)
        || regex_literal_covers_source(source)
    {
        return true;
    }
    if source.as_bytes().first() == Some(&b'{') {
        return compactable_object_container_literal(source);
    }
    if source.as_bytes().first() == Some(&b'[') {
        return compactable_array_container_literal(source);
    }
    simple_runtime_reference_expression(source)
}

pub(crate) fn regex_literal_covers_source(source: &str) -> bool {
    let bytes = source.as_bytes();
    bytes.first() == Some(&b'/')
        && looks_like_regex_literal(bytes, 0)
        && skip_regex_literal(bytes, 0) == bytes.len()
}

pub(crate) fn simple_runtime_reference_expression(source: &str) -> bool {
    let source = source.trim();
    let bytes = source.as_bytes();
    let Some((first, mut cursor)) = parse_identifier(source, 0) else {
        return false;
    };
    if is_js_keyword(first) {
        return false;
    }
    while cursor < bytes.len() {
        if bytes.get(cursor) != Some(&b'.') || bytes.get(cursor + 1) == Some(&b'.') {
            return false;
        }
        cursor += 1;
        let Some((member, next)) = parse_identifier(source, cursor) else {
            return false;
        };
        if is_js_keyword(member) {
            return false;
        }
        cursor = next;
    }
    true
}
