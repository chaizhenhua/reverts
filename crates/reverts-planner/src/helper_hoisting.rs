//! Hoist pure runtime-helper arrow/function-expression `var`s to `function`
//! declarations so they survive circular ESM imports.
//!
//! esbuild scope-hoists every module into one scope, where a helper like the
//! `__esm` lazy initializer `var st = (e, A) => () => (e && (A = e(e = 0)), A)`
//! is defined before any module body runs. Split into separate ESM files, a
//! consumer that imports `st` and calls it EAGERLY at top level (`var uCr =
//! st(() => {…})`, esbuild's lazy module-init thunk) can be evaluated — under a
//! circular import — BEFORE the helper module's `var st = …` line executes, so
//! the imported `st` is still in its temporal dead zone → `st is not a function`.
//!
//! A `function` declaration is initialised at module INSTANTIATION (before any
//! evaluation), so an imported function binding is live even inside an import
//! cycle. Rewriting `var st = (…) => BODY;` to `function st(…) { … }` removes the
//! hazard. Only applied to a single-statement arrow/function-expression `var`
//! whose body uses no `this`/`arguments`/`new.target` (so the function form is
//! observationally identical) — every other shape is left untouched.

/// Rewrite eligible top-level `var/let/const NAME = <arrow|function-expr>;`
/// helper definitions in `source` to hoisted `function NAME(…) { … }`.
pub(crate) fn hoist_arrow_helpers_to_declarations(source: &str) -> String {
    let mut out = Vec::<String>::new();
    for line in source.lines() {
        out.push(hoist_line(line).unwrap_or_else(|| line.to_string()));
    }
    let joined = out.join("\n");
    if source.ends_with('\n') {
        format!("{joined}\n")
    } else {
        joined
    }
}

fn hoist_line(line: &str) -> Option<String> {
    let indent = &line[..line.len() - line.trim_start().len()];
    let trimmed = line.trim();
    let (keyword, rest) = ["var ", "let ", "const "]
        .iter()
        .find_map(|kw| trimmed.strip_prefix(kw).map(|rest| (*kw, rest)))?;
    let _ = keyword;
    let (name, after_name) = rest.split_once('=')?;
    let name = name.trim();
    if !is_identifier(name) {
        return None;
    }
    let body = after_name.trim().strip_suffix(';')?.trim();

    let (params, arrow_body) = split_arrow(body)?;
    // Only hoist the lazy-init WRAPPER helper shape — an arrow whose expression
    // body is itself a function (`(e,A) => () => (…)`, the esbuild `__esm`
    // initializer / lazy-value/-module wrappers). A noop (`() => {}`) or a plain
    // value helper (`() => x`) neither needs hoisting nor should be reshaped (it
    // would churn unrelated helper output), so leave those untouched.
    if block_body(arrow_body).is_some() || !returns_function(arrow_body) {
        return None;
    }
    if uses_dynamic_this(arrow_body) {
        return None;
    }
    let params = normalize_params(params)?;
    Some(format!(
        "{indent}function {name}({params}) {{ return {arrow_body}; }}"
    ))
}

/// Whether an expression is itself a function (arrow or function expression) —
/// the body of a lazy-init wrapper helper.
fn returns_function(expr: &str) -> bool {
    expr.trim_start().starts_with("function") || split_arrow(expr).is_some()
}

/// Split an arrow function `PARAMS => BODY` at its top-level `=>`.
fn split_arrow(expr: &str) -> Option<(&str, &str)> {
    let bytes = expr.as_bytes();
    let mut depth = 0i32;
    let mut string: Option<u8> = None;
    let mut index = 0usize;
    while index + 1 < bytes.len() {
        let byte = bytes[index];
        if let Some(quote) = string {
            if byte == b'\\' {
                index += 2;
                continue;
            }
            if byte == quote {
                string = None;
            }
            index += 1;
            continue;
        }
        match byte {
            b'\'' | b'"' | b'`' => string = Some(byte),
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'=' if depth == 0 && bytes[index + 1] == b'>' => {
                return Some((expr[..index].trim(), expr[index + 2..].trim()));
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// `{ … }` block body → its interior; otherwise `None` (expression body).
fn block_body(body: &str) -> Option<&str> {
    let inner = body.strip_prefix('{')?.strip_suffix('}')?;
    Some(inner.trim())
}

/// Normalise an arrow parameter list to a parenthesised form for `function`.
fn normalize_params(params: &str) -> Option<String> {
    let params = params.trim();
    if let Some(inner) = params.strip_prefix('(').and_then(|p| p.strip_suffix(')')) {
        return Some(inner.trim().to_string());
    }
    // Single bare identifier parameter: `x => …`.
    is_identifier(params).then(|| params.to_string())
}

/// A `function` form rebinds `this`/`arguments`/`new.target`, so converting an
/// arrow that relies on the lexical versions would change behaviour.
fn uses_dynamic_this(body: &str) -> bool {
    contains_word(body, "this") || contains_word(body, "arguments") || body.contains("new.target")
}

fn contains_word(haystack: &str, word: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut start = 0usize;
    while let Some(rel) = haystack[start..].find(word) {
        let pos = start + rel;
        let before_ok = pos == 0 || !is_ident_byte(bytes[pos - 1]);
        let after = pos + word.len();
        let after_ok = after >= bytes.len() || !is_ident_byte(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
        start = pos + word.len();
    }
    false
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

fn is_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().enumerate().all(|(index, byte)| {
            byte == b'_'
                || byte == b'$'
                || byte.is_ascii_alphabetic()
                || (index > 0 && byte.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hoists_esbuild_esm_lazy_initializer_helper() {
        let source = "var st = (e, A) => () => (e && (A = e(e = 0)), A);";
        assert_eq!(
            hoist_arrow_helpers_to_declarations(source),
            "function st(e, A) { return () => (e && (A = e(e = 0)), A); }"
        );
    }

    #[test]
    fn hoists_single_param_wrapper_arrow() {
        let source = "var f = x => () => x;";
        assert_eq!(
            hoist_arrow_helpers_to_declarations(source),
            "function f(x) { return () => x; }"
        );
    }

    #[test]
    fn leaves_noop_arrow_untouched() {
        // A noop helper (`() => {}`) does not need hoisting and must not be
        // reshaped — its output format is relied on elsewhere.
        let source = "var initShared = () => {};";
        assert_eq!(hoist_arrow_helpers_to_declarations(source), source);
    }

    #[test]
    fn leaves_plain_value_arrow_untouched() {
        let source = "var v = () => 42;";
        assert_eq!(hoist_arrow_helpers_to_declarations(source), source);
    }

    #[test]
    fn leaves_function_expression_untouched() {
        let source = "var g = function (a, b) { return a + b; };";
        assert_eq!(hoist_arrow_helpers_to_declarations(source), source);
    }

    #[test]
    fn leaves_this_dependent_wrapper_untouched() {
        let source = "var h = () => () => this.value;";
        assert_eq!(hoist_arrow_helpers_to_declarations(source), source);
    }

    #[test]
    fn leaves_non_function_var_untouched() {
        let source = "var k = { a: 1 };";
        assert_eq!(hoist_arrow_helpers_to_declarations(source), source);
    }

    #[test]
    fn preserves_surrounding_lines() {
        let source = "const a = 1;\nvar st = (e) => () => e;\nconst b = 2;";
        assert_eq!(
            hoist_arrow_helpers_to_declarations(source),
            "const a = 1;\nfunction st(e) { return () => e; }\nconst b = 2;"
        );
    }
}
