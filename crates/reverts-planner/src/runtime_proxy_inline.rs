use std::collections::BTreeSet;

use reverts_ir::BindingName;
use reverts_js::is_ascii_identifier_continue as is_identifier_continue;

use crate::byte_lexer::{
    expect_arrow, find_matching_brace, find_matching_paren, skip_non_code_at, skip_ws,
};
use crate::identifiers::{declaration_keyword_at, keyword_at, parse_identifier};
use crate::{apply_text_edits, identifier_read_facts_in_source, previous_non_ws};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeProxyFunction {
    binding: BindingName,
    target: BindingName,
    span: (usize, usize),
}

pub(crate) fn inline_single_use_runtime_proxy_functions(
    source: &str,
    blocked_bindings: &BTreeSet<BindingName>,
) -> String {
    let mut current = source.to_string();
    for _ in 0..8 {
        let next = inline_single_use_runtime_proxy_functions_once(&current, blocked_bindings);
        if next == current {
            break;
        }
        current = next;
    }
    current
}

pub(crate) fn inline_single_use_runtime_proxy_functions_once(
    source: &str,
    blocked_bindings: &BTreeSet<BindingName>,
) -> String {
    let proxies = runtime_proxy_function_declarations(source)
        .into_iter()
        .filter(|proxy| !blocked_bindings.contains(&proxy.binding))
        .collect::<Vec<_>>();
    if proxies.is_empty() {
        return source.to_string();
    }

    let proxy_targets = proxies
        .iter()
        .map(|proxy| proxy.target.clone())
        .collect::<BTreeSet<_>>();
    let facts = identifier_read_facts_in_source(source);
    let mut edits = Vec::<(usize, usize, String)>::new();
    for proxy in &proxies {
        if proxy_targets.contains(&proxy.binding) {
            continue;
        }
        let reads = facts
            .iter()
            .filter(|fact| fact.name == proxy.binding.as_str())
            .collect::<Vec<_>>();
        let [read] = reads.as_slice() else {
            continue;
        };
        if !read.is_call_callee {
            continue;
        }
        let replacement_end = read.byte_end;
        let replacement_start = replacement_end.saturating_sub(proxy.binding.as_str().len());
        if proxies
            .iter()
            .any(|other| replacement_start >= other.span.0 && replacement_start < other.span.1)
        {
            continue;
        }
        edits.push((proxy.span.0, proxy.span.1, String::new()));
        edits.push((
            replacement_start,
            replacement_end,
            proxy.target.as_str().to_string(),
        ));
    }
    if edits.is_empty() {
        return source.to_string();
    }
    apply_text_edits(source, &edits)
}

pub(crate) fn runtime_proxy_function_declarations(source: &str) -> Vec<RuntimeProxyFunction> {
    let bytes = source.as_bytes();
    let mut proxies = Vec::new();
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
            && keyword_at(source, cursor, "function")
            && keyword_before_position(source, cursor) != Some("async")
            && let Some((proxy, next)) = parse_runtime_proxy_function(source, cursor)
        {
            proxies.push(proxy);
            cursor = next;
            continue;
        }
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && let Some((proxy, next)) = parse_runtime_proxy_arrow_var(source, cursor)
        {
            proxies.push(proxy);
            cursor = next;
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
    proxies
}

pub(crate) fn keyword_before_position(source: &str, position: usize) -> Option<&str> {
    let bytes = source.as_bytes();
    let mut cursor = position;
    let before = loop {
        let before = previous_non_ws(bytes, cursor)?;
        if before > 0 && bytes.get(before - 1) == Some(&b'*') && bytes.get(before) == Some(&b'/') {
            cursor = source[..before - 1].rfind("/*")?;
            continue;
        }
        break before;
    };
    let mut start = before;
    while start > 0 && is_identifier_continue(bytes[start - 1]) {
        start -= 1;
    }
    Some(&source[start..=before])
}

pub(crate) fn parse_runtime_proxy_function(
    source: &str,
    start: usize,
) -> Option<(RuntimeProxyFunction, usize)> {
    let bytes = source.as_bytes();
    let mut cursor = skip_ws(bytes, start + "function".len());
    let (binding, after_binding) = parse_identifier(source, cursor)?;
    cursor = skip_ws(bytes, after_binding);
    if bytes.get(cursor) != Some(&b'(') {
        return None;
    }
    let params_end = find_matching_paren(source, cursor)?;
    let params = parse_simple_identifier_list(&source[cursor + 1..params_end])?;
    cursor = skip_ws(bytes, params_end + 1);
    if bytes.get(cursor) != Some(&b'{') {
        return None;
    }
    let body_end = find_matching_brace(source, cursor)?;
    let body = source[cursor + 1..body_end].trim();
    let (target, args) = parse_proxy_return_call(body)?;
    if target == binding || args != params {
        return None;
    }
    let mut span_end = body_end + 1;
    if bytes.get(span_end) == Some(&b';') {
        span_end += 1;
    }
    if bytes.get(span_end) == Some(&b'\r') {
        span_end += 1;
    }
    if bytes.get(span_end) == Some(&b'\n') {
        span_end += 1;
    }
    Some((
        RuntimeProxyFunction {
            binding: BindingName::new(binding),
            target: BindingName::new(target),
            span: (start, span_end),
        },
        span_end,
    ))
}

pub(crate) fn parse_runtime_proxy_arrow_var(
    source: &str,
    start: usize,
) -> Option<(RuntimeProxyFunction, usize)> {
    let bytes = source.as_bytes();
    let (_keyword, keyword_len) = declaration_keyword_at(source, start)?;
    let mut cursor = skip_ws(bytes, start + keyword_len);
    let (binding, after_binding) = parse_identifier(source, cursor)?;
    cursor = skip_ws(bytes, after_binding);
    if bytes.get(cursor) != Some(&b'=') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    let (params, after_params) = parse_arrow_proxy_params(source, cursor)?;
    cursor = skip_ws(bytes, after_params);
    cursor = expect_arrow(bytes, cursor)?;
    cursor = skip_ws(bytes, cursor);
    let (target, after_target) = parse_identifier(source, cursor)?;
    if target == binding {
        return None;
    }
    let open = skip_ws(bytes, after_target);
    if bytes.get(open) != Some(&b'(') {
        return None;
    }
    let close = find_matching_paren(source, open)?;
    let args = parse_simple_identifier_list(&source[open + 1..close])?;
    if args != params {
        return None;
    }
    cursor = skip_ws(bytes, close + 1);
    if bytes.get(cursor) != Some(&b';') {
        return None;
    }
    let mut span_end = cursor + 1;
    if bytes.get(span_end) == Some(&b'\r') {
        span_end += 1;
    }
    if bytes.get(span_end) == Some(&b'\n') {
        span_end += 1;
    }
    Some((
        RuntimeProxyFunction {
            binding: BindingName::new(binding),
            target: BindingName::new(target),
            span: (start, span_end),
        },
        span_end,
    ))
}

pub(crate) fn parse_arrow_proxy_params(source: &str, start: usize) -> Option<(Vec<String>, usize)> {
    let bytes = source.as_bytes();
    if bytes.get(start) == Some(&b'(') {
        let close = find_matching_paren(source, start)?;
        let params = parse_simple_identifier_list(&source[start + 1..close])?;
        return Some((params, close + 1));
    }
    let (param, after_param) = parse_identifier(source, start)?;
    Some((vec![param.to_string()], after_param))
}

pub(crate) fn parse_proxy_return_call(body: &str) -> Option<(&str, Vec<String>)> {
    let body = body.trim().strip_prefix("return ")?.trim();
    let body = body.trim_end_matches(';').trim();
    let (target, after_target) = parse_identifier(body, 0)?;
    let open = skip_ws(body.as_bytes(), after_target);
    if body.as_bytes().get(open) != Some(&b'(') {
        return None;
    }
    let close = find_matching_paren(body, open)?;
    if skip_ws(body.as_bytes(), close + 1) != body.len() {
        return None;
    }
    let args = parse_simple_identifier_list(&body[open + 1..close])?;
    Some((target, args))
}

pub(crate) fn parse_simple_identifier_list(source: &str) -> Option<Vec<String>> {
    let source = source.trim();
    if source.is_empty() {
        return Some(Vec::new());
    }
    source
        .split(',')
        .map(|part| {
            let part = part.trim();
            let (identifier, end) = parse_identifier(part, 0)?;
            (end == part.len()).then(|| identifier.to_string())
        })
        .collect()
}
