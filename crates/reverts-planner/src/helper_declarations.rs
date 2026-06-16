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
        let mut cursor = start + keyword.len();
        cursor = skip_ws(bytes, cursor);
        let Some((binding, after_binding)) = parse_identifier(source, cursor) else {
            index = start + keyword.len();
            continue;
        };
        cursor = skip_ws(bytes, after_binding);
        if bytes.get(cursor) != Some(&b'=') {
            index = start + keyword.len();
            continue;
        }
        cursor = skip_ws(bytes, cursor + 1);
        let Some((callee, after_callee)) = parse_identifier(source, cursor) else {
            index = cursor.saturating_add(1);
            continue;
        };
        if callee != helper {
            index = after_callee;
            continue;
        }
        cursor = skip_ws(bytes, after_callee);
        if bytes.get(cursor) != Some(&b'(') {
            index = after_callee;
            continue;
        }
        let parsed = match kind {
            HelperDeclarationKind::CommonJsWrapper => {
                parse_commonjs_wrapper_replacement(source, cursor + 1, binding)
            }
            HelperDeclarationKind::LazyInitializer => {
                parse_lazy_initializer_replacement(source, cursor + 1, binding)
            }
        };
        let Some((replacement, end)) = parsed else {
            index = after_callee;
            continue;
        };
        return Some(HelperDeclaration {
            start,
            end,
            replacement,
        });
    }
    None
}

pub(crate) fn parse_commonjs_wrapper_replacement(
    source: &str,
    mut cursor: usize,
    binding: &str,
) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'(') {
        return None;
    }
    let params_end = find_byte(bytes, cursor + 1, b')')?;
    let params = source[cursor + 1..params_end]
        .split(',')
        .map(str::trim)
        .filter(|param| !param.is_empty())
        .collect::<Vec<_>>();
    if params.is_empty() {
        return None;
    }
    cursor = skip_ws(bytes, params_end + 1);
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
