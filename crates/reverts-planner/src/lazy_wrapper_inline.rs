use reverts_ir::BindingName;

use crate::byte_lexer::{expect_arrow, find_matching_paren, skip_non_code_at, skip_ws};
use crate::identifiers::{declaration_keyword_at, keyword_at, parse_identifier};
use crate::{
    apply_text_edits, contains_identifier_reference, local_bindings_in_source, previous_non_ws,
    source_contains_top_level_call,
};

pub(crate) fn inline_remaining_lazy_value_wrappers(source: &str) -> (String, bool) {
    inline_remaining_lazy_value_wrappers_with_options(source, false)
}

pub(crate) fn inline_remaining_lazy_value_wrappers_allowing_assignments(
    source: &str,
) -> (String, bool) {
    inline_remaining_lazy_value_wrappers_with_options(source, true)
}

pub(crate) fn localize_lazy_value_source(source: &str) -> Option<String> {
    let (localized, changed) = inline_remaining_lazy_value_wrappers_allowing_assignments(source);
    (changed && !source_contains_top_level_call(localized.as_str(), "lazyValue"))
        .then_some(localized)
}

pub(crate) fn inline_remaining_lazy_value_wrappers_with_options(
    source: &str,
    allow_assignment_factories: bool,
) -> (String, bool) {
    let bytes = source.as_bytes();
    let mut edits = Vec::<(usize, usize, String)>::new();
    let helper_name = local_lazy_value_helper_name(source);
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
            && let Some((decl, after)) =
                try_parse_lazy_value_wrapper_declaration(source, cursor, allow_assignment_factories)
        {
            edits.push((decl.callee_span.0, decl.callee_span.1, helper_name.clone()));
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
        return (source.to_string(), false);
    }
    let source = apply_text_edits(source, &edits);
    (
        format!("{}\n{source}", local_lazy_value_helper_source(&helper_name)),
        true,
    )
}

#[derive(Debug, Clone)]
pub(crate) struct ParsedLazyValueWrapper {
    callee_span: (usize, usize),
}

pub(crate) fn try_parse_lazy_value_wrapper_declaration(
    source: &str,
    start: usize,
    allow_assignment_factory: bool,
) -> Option<(ParsedLazyValueWrapper, usize)> {
    let (_keyword, after_keyword) = declaration_keyword_at(source, start)?;
    let bytes = source.as_bytes();
    let mut cursor = skip_ws(bytes, start + after_keyword);
    let (_binding, after_binding) = parse_identifier(source, cursor)?;
    cursor = skip_ws(bytes, after_binding);
    if bytes.get(cursor) != Some(&b'=') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    let callee_start = cursor;
    if !keyword_at(source, cursor, "lazyValue") {
        return None;
    }
    cursor += "lazyValue".len();
    let callee_end = cursor;
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'(') {
        return None;
    }
    let call_open = cursor;
    let call_close = find_matching_paren(source, call_open)?;
    let factory = source[call_open + 1..call_close].trim();
    if factory.is_empty() {
        return None;
    }
    if !lazy_value_factory_is_zero_arg_arrow(factory) {
        return None;
    }
    // Keep assignment-heavy lazy initializers in their canonical
    // `lazyValue(...)` shape for the runtime-folding pass. Those bodies are
    // often the writer side of runtime mutable bindings; inlining them before
    // fold planning would hide the `X = ...` writes and prevent the safer
    // runtime relocation/folding machinery from seeing them.
    if !allow_assignment_factory && lazy_value_factory_contains_assignment(factory) {
        return None;
    }
    let after_call = skip_ws(bytes, call_close + 1);
    if bytes.get(after_call) != Some(&b';') {
        return None;
    }
    let stmt_end = after_call + 1;
    Some((
        ParsedLazyValueWrapper {
            callee_span: (callee_start, callee_end),
        },
        stmt_end,
    ))
}

pub(crate) fn lazy_value_factory_contains_assignment(factory: &str) -> bool {
    let Some(arrow) = factory.find("=>") else {
        return true;
    };
    let body = &factory[arrow + 2..];
    let bytes = body.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(body, cursor) {
            cursor = next;
            continue;
        }
        if bytes[cursor] != b'=' {
            cursor += 1;
            continue;
        }
        let prev = previous_non_ws(bytes, cursor);
        let next = skip_ws(bytes, cursor + 1);
        let prev_is_operator =
            prev.is_some_and(|idx| matches!(bytes[idx], b'=' | b'!' | b'<' | b'>'));
        let next_is_operator = matches!(bytes.get(next), Some(b'=') | Some(b'>'));
        if !prev_is_operator && !next_is_operator {
            return true;
        }
        cursor += 1;
    }
    false
}

pub(crate) fn lazy_value_factory_is_zero_arg_arrow(factory: &str) -> bool {
    let bytes = factory.as_bytes();
    let mut cursor = skip_ws(bytes, 0);
    if bytes.get(cursor) != Some(&b'(') {
        return false;
    }
    let Some(params_end) = find_matching_paren(factory, cursor) else {
        return false;
    };
    if !factory[cursor + 1..params_end].trim().is_empty() {
        return false;
    }
    cursor = skip_ws(bytes, params_end + 1);
    expect_arrow(bytes, cursor).is_some()
}

pub(crate) fn local_lazy_value_helper_name(source: &str) -> String {
    let mut name = "_$l".to_string();
    let mut suffix = 0usize;
    let local_bindings = local_bindings_in_source(source);
    while local_bindings.contains(name.as_str())
        || contains_identifier_reference(source, name.as_str())
    {
        suffix += 1;
        name = format!("_$l{suffix}");
    }
    name
}

pub(crate) fn local_lazy_value_helper_source(helper_name: &str) -> String {
    format!("var {helper_name}=(_$f,_$v)=>()=>(_$f&&(_$v=_$f(_$f=0)),_$v);")
}

pub(crate) fn inline_remaining_lazy_module_wrappers(source: &str) -> (String, bool) {
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
            && let Some((decl, after)) = try_parse_lazy_module_wrapper_declaration(source, cursor)
        {
            edits.push((
                decl.span.0,
                decl.span.1,
                inline_lazy_module_wrapper_replacement(&decl),
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
        return (source.to_string(), false);
    }
    (apply_text_edits(source, &edits), true)
}

#[derive(Debug, Clone)]
pub(crate) struct ParsedLazyModuleWrapper<'a> {
    binding: BindingName,
    factory: &'a str,
    span: (usize, usize),
}

pub(crate) fn try_parse_lazy_module_wrapper_declaration(
    source: &str,
    start: usize,
) -> Option<(ParsedLazyModuleWrapper<'_>, usize)> {
    let (_keyword, after_keyword) = declaration_keyword_at(source, start)?;
    let bytes = source.as_bytes();
    let mut cursor = skip_ws(bytes, start + after_keyword);
    let (binding, after_binding) = parse_identifier(source, cursor)?;
    cursor = skip_ws(bytes, after_binding);
    if bytes.get(cursor) != Some(&b'=') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    if !keyword_at(source, cursor, "lazyModule") {
        return None;
    }
    cursor += "lazyModule".len();
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'(') {
        return None;
    }
    let call_open = cursor;
    let call_close = find_matching_paren(source, call_open)?;
    let factory = source[call_open + 1..call_close].trim();
    if factory.is_empty() {
        return None;
    }
    let after_call = skip_ws(bytes, call_close + 1);
    if bytes.get(after_call) != Some(&b';') {
        return None;
    }
    let stmt_end = after_call + 1;
    Some((
        ParsedLazyModuleWrapper {
            binding: BindingName::new(binding),
            factory,
            span: (start, stmt_end),
        },
        stmt_end,
    ))
}

pub(crate) fn inline_lazy_module_wrapper_replacement(decl: &ParsedLazyModuleWrapper<'_>) -> String {
    let binding = decl.binding.as_str();
    format!(
        "var {binding} = (() => {{ let _$cached; return () => {{ if (_$cached) return _$cached.exports; var _$module = _$cached = {{ exports: {{}} }}; ({factory})(_$module.exports, _$module); return _$module.exports; }}; }})();",
        factory = decl.factory,
    )
}
