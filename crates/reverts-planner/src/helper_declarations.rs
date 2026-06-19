use crate::byte_lexer::{expect_arrow, find_byte, find_matching_brace, skip_ws};
use crate::identifiers::{find_declaration_keyword, parse_identifier};

pub(crate) fn lower_commonjs_wrapper_helper(source: &str, helper: &str) -> Option<String> {
    lower_helper_declarations(source, helper, HelperDeclarationKind::CommonJsWrapper)
}

pub(crate) fn lower_lazy_initializer_helper(source: &str, helper: &str) -> Option<String> {
    lower_helper_declarations(source, helper, HelperDeclarationKind::LazyInitializer)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HelperDeclarationKind {
    CommonJsWrapper,
    LazyInitializer,
}

pub(crate) fn lower_helper_declarations(
    source: &str,
    helper: &str,
    kind: HelperDeclarationKind,
) -> Option<String> {
    let mut output = String::new();
    let mut index = 0;
    let mut changed = false;
    while let Some(declaration) = find_helper_declaration(source, index, helper, kind) {
        output.push_str(&source[index..declaration.start]);
        output.push_str(declaration.replacement.as_str());
        index = declaration.end;
        changed = true;
    }
    if changed {
        output.push_str(&source[index..]);
        Some(output)
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HelperDeclaration {
    start: usize,
    end: usize,
    replacement: String,
}

pub(crate) fn find_helper_declaration(
    source: &str,
    from: usize,
    helper: &str,
    kind: HelperDeclarationKind,
) -> Option<HelperDeclaration> {
    let bytes = source.as_bytes();
    let mut index = from;
    while index < bytes.len() {
        let (start, keyword) = find_declaration_keyword(source, index)?;
        let cursor = start + keyword.len();
        if let Some((replacement, end)) = parse_helper_declarator_list(source, cursor, helper, kind)
        {
            return Some(HelperDeclaration {
                start,
                end,
                replacement,
            });
        }
        // Not a (fully) lowerable helper declaration at this keyword. Advance
        // just past the keyword so the next scan picks up the following
        // `var`/`let`/`const` without re-matching this one.
        index = start + keyword.len();
    }
    None
}

/// Parse every comma-separated declarator in a single `var`/`let`/`const`
/// statement, requiring each to be `<binding> = HELPER(<args>)`. esbuild
/// co-declares several module/value handles in one statement
/// (`var a = U(...), b = U(...)`), so lowering only the first declarator would
/// emit malformed JS (`var a = lazyModule(...);, b = U(...)`) and leave the
/// co-declared handles wrapped by nobody. Each declarator becomes its own
/// `var <binding> = lazyModule(...);` statement so that the downstream
/// single-declarator delazify / inline passes can collapse each independently.
///
/// Returns `None` (leaving the statement untouched) unless the *first*
/// declarator is a helper call — that signals this keyword is not a helper
/// declaration — or if any *later* declarator is not a helper call, in which
/// case lowering would risk severing the comma list, so we conservatively
/// decline the whole statement.
fn parse_helper_declarator_list(
    source: &str,
    cursor_after_keyword: usize,
    helper: &str,
    kind: HelperDeclarationKind,
) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    let mut replacements: Vec<String> = Vec::new();
    let mut cursor = cursor_after_keyword;
    loop {
        let (replacement, end) = parse_one_helper_declarator(source, cursor, helper, kind)?;
        replacements.push(replacement);
        let after = skip_ws(bytes, end);
        if bytes.get(after) == Some(&b',') {
            cursor = after + 1;
            continue;
        }
        return Some((replacements.join(" "), end));
    }
}

/// Parse a single `<binding> = HELPER(<args>)` declarator starting at `start`,
/// returning its standalone `var <binding> = lazy…(...);` replacement and the
/// byte offset just past the declarator (after the call's `)` and any trailing
/// `;`). Returns `None` for any other declarator shape.
fn parse_one_helper_declarator(
    source: &str,
    start: usize,
    helper: &str,
    kind: HelperDeclarationKind,
) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    let cursor = skip_ws(bytes, start);
    let (binding, after_binding) = parse_identifier(source, cursor)?;
    let cursor = skip_ws(bytes, after_binding);
    if bytes.get(cursor) != Some(&b'=') {
        return None;
    }
    let cursor = skip_ws(bytes, cursor + 1);
    let (callee, after_callee) = parse_identifier(source, cursor)?;
    if callee != helper {
        return None;
    }
    let cursor = skip_ws(bytes, after_callee);
    if bytes.get(cursor) != Some(&b'(') {
        return None;
    }
    match kind {
        HelperDeclarationKind::CommonJsWrapper => {
            parse_commonjs_wrapper_replacement(source, cursor + 1, binding)
        }
        HelperDeclarationKind::LazyInitializer => {
            parse_lazy_initializer_replacement(source, cursor + 1, binding)
        }
    }
}

pub(crate) fn parse_commonjs_wrapper_replacement(
    source: &str,
    mut cursor: usize,
    binding: &str,
) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    cursor = skip_ws(bytes, cursor);
    // esbuild minified arrows: a single-identifier parameter list often loses
    // its parens (`e=>{...}` instead of `(e)=>{...}`). Accept both forms.
    let (params, after_params): (Vec<&str>, usize) = if bytes.get(cursor) == Some(&b'(') {
        let params_end = find_byte(bytes, cursor + 1, b')')?;
        let parsed = source[cursor + 1..params_end]
            .split(',')
            .map(str::trim)
            .filter(|param| !param.is_empty())
            .collect::<Vec<_>>();
        (parsed, params_end + 1)
    } else if let Some((ident, after_ident)) = crate::identifiers::parse_identifier(source, cursor)
    {
        (vec![ident], after_ident)
    } else {
        return None;
    };
    if params.is_empty() {
        return None;
    }
    cursor = skip_ws(bytes, after_params);
    cursor = expect_arrow(bytes, cursor)?;
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'{') {
        return None;
    }
    let body_end = find_matching_brace(source, cursor)?;
    let body = source[cursor + 1..body_end].trim();
    let end = parse_helper_call_end(bytes, body_end + 1)?;
    let exports = params[0];
    let module_alias = params.get(1).copied();
    let parameter_list = match module_alias {
        Some(module) => format!("({exports}, {module})"),
        None => format!("({exports})"),
    };
    Some((
        format!("var {binding} = lazyModule({parameter_list} => {{\n{body}\n}});"),
        end,
    ))
}

pub(crate) fn parse_lazy_initializer_replacement(
    source: &str,
    mut cursor: usize,
    binding: &str,
) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'(') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    if bytes.get(cursor) != Some(&b')') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    cursor = expect_arrow(bytes, cursor)?;
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'{') {
        return None;
    }
    let body_end = find_matching_brace(source, cursor)?;
    let body = source[cursor + 1..body_end].trim();
    let end = parse_helper_call_end(bytes, body_end + 1)?;
    Some((
        format!("var {binding} = lazyValue(() => {{\n{body}\n}});"),
        end,
    ))
}

pub(crate) fn parse_helper_call_end(bytes: &[u8], mut cursor: usize) -> Option<usize> {
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b')') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    if bytes.get(cursor) == Some(&b';') {
        cursor += 1;
    }
    Some(cursor)
}
