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

/// Heuristic: does this identifier look machine-generated / minified (and thus
/// a semantic-naming target), rather than an already-meaningful name?
///
/// Conservative and tunable. Evidence from real esbuild bundles: minified
/// bindings look like `a`, `e`, `U$`, `sG`, `Whr`, `m3A`, `K3A`, `t1`;
/// meaningful names look like `emitClose`, `WebSocket`, `Duplex`, `Map`, `get`.
#[must_use]
pub fn is_minified_identifier(name: &str) -> bool {
    let name = name.trim();
    if name.is_empty() {
        return false;
    }
    let len = name.chars().count();
    if len <= 2 {
        return true;
    }
    // Minifier sigils (esbuild/terser emit `$`-laden names).
    if name.contains('$') {
        return true;
    }
    let has_vowel = name
        .chars()
        .any(|ch| matches!(ch.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u'));
    let has_digit = name.chars().any(|ch| ch.is_ascii_digit());
    // Short names that lack a vowel or splice in a digit read as machine names
    // (`Whr`, `jhr`, `m3A`), while short real words keep a vowel (`Map`, `get`).
    len <= 4 && (!has_vowel || has_digit)
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
pub fn read_identifier_at(source: &str, start: usize) -> Option<&str> {
    let bytes = source.as_bytes();
    let first = *bytes.get(start)?;
    if !is_ascii_identifier_start(first) {
        return None;
    }
    let mut end = start + 1;
    while bytes
        .get(end)
        .is_some_and(|byte| is_ascii_identifier_continue(*byte))
    {
        end += 1;
    }
    source.get(start..end)
}

#[must_use]
pub fn read_identifier_with_end_at(source: &str, start: usize) -> Option<(&str, usize)> {
    let identifier = read_identifier_at(source, start)?;
    Some((identifier, start + identifier.len()))
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

#[cfg(test)]
mod minified_tests {
    use super::is_minified_identifier;

    #[test]
    fn flags_machine_generated_names() {
        for name in [
            "a", "e", "t1", "U$", "sG", "ND", "hx", "Whr", "jhr", "m3A", "K3A",
        ] {
            assert!(is_minified_identifier(name), "{name} should be minified");
        }
    }

    #[test]
    fn keeps_meaningful_names() {
        for name in [
            "emitClose",
            "duplexOnEnd",
            "WebSocket",
            "Duplex",
            "createRequire",
            "tokenize",
            "Map",
            "get",
            "key",
        ] {
            assert!(!is_minified_identifier(name), "{name} should be meaningful");
        }
    }
}
