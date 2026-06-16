//! Byte-walking JS lexer helpers shared by the planner's source-rewriting passes.
//!
//! Several planner passes need to step over recovered JS source while
//! respecting string/template/regex/comment boundaries — for example to
//! find the matching `)` of a setter call, or to skip past a multi-line
//! string when locating a brace's partner. We deliberately avoid OXC
//! here because:
//!
//! 1. These helpers run inside larger walks that already produced text
//!    from OXC codegen and only need to find structural boundaries; a
//!    re-parse would be wasteful and would change span identity.
//! 2. They are pure byte-level functions with no AST allocations.
//!
//! ADR 0001 still applies: every `Preserve…` decision and large-scale
//! rewrite goes through OXC. These helpers are used purely for delimiter
//! matching on already-emitted output, not for parsing user source.

use reverts_js::{
    is_ascii_identifier_continue as is_identifier_continue, skip_block_comment, skip_line_comment,
};

pub(crate) fn expect_arrow(bytes: &[u8], cursor: usize) -> Option<usize> {
    (bytes.get(cursor) == Some(&b'=') && bytes.get(cursor + 1) == Some(&b'>')).then_some(cursor + 2)
}

pub(crate) fn find_byte(bytes: &[u8], mut cursor: usize, target: u8) -> Option<usize> {
    while cursor < bytes.len() {
        if bytes[cursor] == target {
            return Some(cursor);
        }
        cursor += 1;
    }
    None
}

pub(crate) fn find_matching_brace(source: &str, open: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut cursor = open;
    let mut depth = 0usize;
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
                cursor = skip_regex_literal(bytes, cursor);
            }
            b'{' => {
                depth += 1;
                cursor += 1;
            }
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(cursor);
                }
                cursor += 1;
            }
            _ => cursor += 1,
        }
    }
    None
}

pub(crate) fn find_matching_bracket(source: &str, open: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut cursor = open;
    let mut depth = 0usize;
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
                cursor = skip_regex_literal(bytes, cursor);
            }
            b'[' => {
                depth += 1;
                cursor += 1;
            }
            b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(cursor);
                }
                cursor += 1;
            }
            _ => cursor += 1,
        }
    }
    None
}

pub(crate) fn find_matching_paren(source: &str, open: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut cursor = open;
    let mut depth = 0usize;
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
                cursor = skip_regex_literal(bytes, cursor);
            }
            b'(' => {
                depth += 1;
                cursor += 1;
            }
            b')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(cursor);
                }
                cursor += 1;
            }
            _ => cursor += 1,
        }
    }
    None
}

pub(crate) fn skip_ws(bytes: &[u8], mut cursor: usize) -> usize {
    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    cursor
}

pub(crate) fn skip_ws_and_comments(bytes: &[u8], mut cursor: usize, limit: usize) -> usize {
    let limit = limit.min(bytes.len());
    loop {
        while cursor < limit && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor + 1 < limit && bytes[cursor] == b'/' && bytes[cursor + 1] == b'/' {
            cursor = skip_line_comment(bytes, cursor + 2).min(limit);
            continue;
        }
        if cursor + 1 < limit && bytes[cursor] == b'/' && bytes[cursor + 1] == b'*' {
            cursor = skip_block_comment(bytes, cursor + 2).min(limit);
            continue;
        }
        return cursor;
    }
}

pub(crate) fn skip_quoted(bytes: &[u8], start: usize, quote: u8) -> usize {
    if quote == b'`' {
        return skip_template_literal(bytes, start);
    }
    let mut cursor = start + 1;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\\' {
            cursor += 2;
            continue;
        }
        if bytes[cursor] == quote {
            return cursor + 1;
        }
        cursor += 1;
    }
    bytes.len()
}

pub(crate) fn skip_template_literal(bytes: &[u8], start: usize) -> usize {
    let mut cursor = start + 1;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => cursor += 2,
            b'`' => return cursor + 1,
            b'$' if bytes.get(cursor + 1) == Some(&b'{') => {
                cursor = skip_template_expression(bytes, cursor + 2);
            }
            _ => cursor += 1,
        }
    }
    bytes.len()
}

pub(crate) fn skip_template_expression(bytes: &[u8], mut cursor: usize) -> usize {
    let mut depth = 1usize;
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
                cursor = skip_regex_literal(bytes, cursor);
            }
            b'{' => {
                depth += 1;
                cursor += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                cursor += 1;
                if depth == 0 {
                    return cursor;
                }
            }
            _ => cursor += 1,
        }
    }
    bytes.len()
}

pub(crate) fn looks_like_regex_literal(bytes: &[u8], slash: usize) -> bool {
    if bytes.get(slash + 1).is_none() {
        return false;
    }
    let mut cursor = slash;
    while cursor > 0 {
        cursor -= 1;
        if bytes[cursor].is_ascii_whitespace() {
            continue;
        }
        if is_identifier_continue(bytes[cursor]) {
            let end = cursor + 1;
            while cursor > 0 && is_identifier_continue(bytes[cursor - 1]) {
                cursor -= 1;
            }
            return matches!(
                std::str::from_utf8(&bytes[cursor..end]).unwrap_or_default(),
                "return" | "throw" | "case" | "yield" | "delete" | "void" | "typeof"
            );
        }
        return matches!(
            bytes[cursor],
            b'(' | b'='
                | b':'
                | b'['
                | b','
                | b'!'
                | b'?'
                | b'{'
                | b';'
                | b'&'
                | b'|'
                | b'+'
                | b'-'
                | b'*'
                | b'%'
                | b'~'
                | b'^'
                | b'<'
                | b'>'
        );
    }
    true
}

/// Step past any non-code byte sequence starting at `cursor` — strings,
/// template literals, line/block comments, regex literals. Returns
/// `None` when `cursor` doesn't begin one of those (so callers can fall
/// back to normal byte processing).
pub(crate) fn skip_non_code_at(source: &str, cursor: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let byte = *bytes.get(cursor)?;
    match byte {
        b'\'' | b'"' => Some(skip_quoted(bytes, cursor, byte)),
        b'`' => Some(skip_template_literal(bytes, cursor)),
        b'/' if bytes.get(cursor + 1) == Some(&b'/') => Some(skip_line_comment(bytes, cursor + 2)),
        b'/' if bytes.get(cursor + 1) == Some(&b'*') => Some(skip_block_comment(bytes, cursor + 2)),
        b'/' if looks_like_regex_literal(bytes, cursor) => Some(skip_regex_literal(bytes, cursor)),
        _ => None,
    }
}

/// Walk `source` (the bytes between a call's `(` and `)`) and return
/// `true` iff it contains exactly one top-level expression — i.e., no
/// top-level comma outside nested parens/brackets/braces/strings. Used
/// to gate the setter-call inliner: multi-argument setter calls have
/// different semantics from a comma-folded assignment expression, so
/// they stay as function calls.
pub(crate) fn arg_text_is_single_expression(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut cursor = 0;
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
                cursor = skip_regex_literal(bytes, cursor);
            }
            b'(' => {
                let Some(close) = find_matching_paren(source, cursor) else {
                    return false;
                };
                cursor = close + 1;
            }
            b'[' => {
                let Some(close) = find_matching_bracket(source, cursor) else {
                    return false;
                };
                cursor = close + 1;
            }
            b'{' => {
                let Some(close) = find_matching_brace(source, cursor) else {
                    return false;
                };
                cursor = close + 1;
            }
            b',' => return false,
            _ => cursor += 1,
        }
    }
    true
}

pub(crate) fn skip_regex_literal(bytes: &[u8], start: usize) -> usize {
    let mut cursor = start + 1;
    let mut in_class = false;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => cursor += 2,
            b'[' => {
                in_class = true;
                cursor += 1;
            }
            b']' => {
                in_class = false;
                cursor += 1;
            }
            b'/' if !in_class => {
                cursor += 1;
                while cursor < bytes.len() && bytes[cursor].is_ascii_alphabetic() {
                    cursor += 1;
                }
                return cursor;
            }
            _ => cursor += 1,
        }
    }
    bytes.len()
}
