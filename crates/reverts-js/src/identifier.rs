pub use reverts_ir::{
    is_ascii_identifier_continue, is_ascii_identifier_start, is_identifier_like_ascii,
};

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
    u8::try_from(ch).is_ok_and(is_ascii_identifier_start)
}

#[must_use]
pub fn is_identifier_part(ch: char) -> bool {
    u8::try_from(ch).is_ok_and(is_ascii_identifier_continue)
}

#[must_use]
pub fn is_js_keyword(value: &str) -> bool {
    matches!(
        value,
        "async"
            | "await"
            | "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "default"
            | "do"
            | "else"
            | "export"
            | "extends"
            | "false"
            | "finally"
            | "for"
            | "from"
            | "function"
            | "if"
            | "import"
            | "in"
            | "let"
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
            | "undefined"
            | "var"
            | "void"
            | "while"
            | "with"
            | "yield"
    )
}

#[must_use]
pub fn is_valid_static_member_property_name(value: &str) -> bool {
    is_identifier_like_ascii(value) && !is_reserved_word(value)
}

#[must_use]
pub fn skip_line_comment(bytes: &[u8], mut cursor: usize) -> usize {
    while cursor < bytes.len() && bytes[cursor] != b'\n' {
        cursor += 1;
    }
    cursor
}

#[must_use]
pub fn skip_block_comment(bytes: &[u8], mut cursor: usize) -> usize {
    while cursor + 1 < bytes.len() {
        if bytes[cursor] == b'*' && bytes[cursor + 1] == b'/' {
            return cursor + 2;
        }
        cursor += 1;
    }
    bytes.len()
}

#[must_use]
pub fn read_quoted_string_at(source: &str, start: usize) -> Option<(String, usize)> {
    let quote = *source.as_bytes().get(start)?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    let mut escaped = false;
    let mut out = String::new();
    for (offset, ch) in source[start + 1..].char_indices() {
        if escaped {
            out.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch as u8 == quote {
            return Some((out, start + 1 + offset + ch.len_utf8()));
        }
        out.push(ch);
    }
    None
}

fn is_reserved_word(value: &str) -> bool {
    matches!(
        value,
        "await"
            | "arguments"
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
            | "eval"
            | "export"
            | "extends"
            | "false"
            | "finally"
            | "for"
            | "function"
            | "if"
            | "implements"
            | "import"
            | "in"
            | "instanceof"
            | "interface"
            | "let"
            | "new"
            | "null"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "return"
            | "static"
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
