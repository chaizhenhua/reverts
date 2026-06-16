//! Rewrites direct writes to runtime-helper bindings into setter calls.
//!
//! Once a helper binding moves into the cross-module runtime helpers
//! file, consumers can no longer write to it directly — ESM forbids
//! assigning to imported bindings. The planner emits a
//! `__reverts_set_X(value)` function alongside each helper; this pass
//! walks recovered source and rewrites every plain `X = …` /
//! `X++` / `++X` / `X--` / `--X` into the corresponding setter call so
//! the helper binding stays mutable from the consumer's point of view.
//!
//! The walker is byte-level for performance and so that the rewrite
//! preserves byte-exact span identity on the unchanged portions of
//! source. It deliberately skips:
//!
//! - strings / regex / comments (delimiter-aware)
//! - member accesses (`obj.X = …`, `arr[X] = …`)
//! - same-name local declarations inside the source slice
//! - destructuring patterns — those are routed to
//!   `rewrite_object_destructuring_helper_writes` /
//!   `rewrite_array_destructuring_helper_writes` because the rewrite
//!   has to preserve the pattern's other targets.
//!
//! `runtime_helper_update_expression` constructs an IIFE that produces
//! the same numeric result and side-effect ordering as the original
//! `++`/`--` operator while routing the mutation through the setter.
//! Both Prefix and Postfix variants are encoded explicitly so the audit
//! sees the right sequence-points.

use std::collections::BTreeSet;

use reverts_ir::BindingName;
use reverts_js::{
    is_ascii_identifier_continue as is_identifier_continue,
    is_ascii_identifier_start as is_identifier_start, is_js_keyword, skip_block_comment,
    skip_line_comment,
};

use crate::byte_lexer::{looks_like_regex_literal, skip_quoted, skip_regex_literal, skip_ws};
use crate::identifiers::parse_identifier;
use crate::statements::runtime_helper_setter_name;
use crate::{
    rewrite_array_destructuring_helper_writes, rewrite_object_destructuring_helper_writes,
    variable_declaration_binding_starts,
};

#[derive(Clone, Copy)]
pub(crate) enum UpdateOperator {
    Increment,
    Decrement,
}

impl UpdateOperator {
    fn source(self) -> &'static str {
        match self {
            Self::Increment => "++",
            Self::Decrement => "--",
        }
    }
}

pub(crate) fn update_operator_at(bytes: &[u8], cursor: usize) -> Option<UpdateOperator> {
    match (bytes.get(cursor), bytes.get(cursor + 1)) {
        (Some(b'+'), Some(b'+')) => Some(UpdateOperator::Increment),
        (Some(b'-'), Some(b'-')) => Some(UpdateOperator::Decrement),
        _ => None,
    }
}

pub(crate) fn is_simple_update_target(source: &str, start: usize, end: usize) -> bool {
    let bytes = source.as_bytes();
    let identifier = &source[start..end];
    if is_js_keyword(identifier) {
        return false;
    }
    if start
        .checked_sub(1)
        .and_then(|index| bytes.get(index))
        .is_some_and(|byte| matches!(*byte, b'#' | b'.'))
    {
        return false;
    }
    let after = skip_ws(bytes, end);
    !matches!(bytes.get(after), Some(b'.' | b'['))
}

pub(crate) fn rewrite_runtime_helper_writes(
    source: &str,
    helpers: &BTreeSet<BindingName>,
) -> String {
    let mut output = String::new();
    let mut last = 0usize;
    let mut changed = false;
    let declaration_bindings = variable_declaration_binding_starts(source);
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
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
            b'+' | b'-' => {
                let Some(operator) = update_operator_at(bytes, cursor) else {
                    cursor += 1;
                    continue;
                };
                let target_start = skip_ws(bytes, cursor + 2);
                let Some((identifier, target_end)) = parse_identifier(source, target_start) else {
                    cursor += 1;
                    continue;
                };
                if declaration_bindings.contains(&target_start)
                    || !is_simple_update_target(source, target_start, target_end)
                {
                    cursor += 1;
                    continue;
                }
                let Some(helper) = helpers
                    .iter()
                    .find(|binding| binding.as_str() == identifier)
                else {
                    cursor += 1;
                    continue;
                };
                output.push_str(&source[last..cursor]);
                output.push_str(
                    runtime_helper_update_expression(helper, operator, UpdatePosition::Prefix)
                        .as_str(),
                );
                last = target_end;
                cursor = target_end;
                changed = true;
            }
            b'{' => {
                if let Some((end, replacement)) =
                    rewrite_object_destructuring_helper_writes(source, cursor, helpers)
                {
                    output.push_str(&source[last..cursor]);
                    output.push_str(replacement.as_str());
                    last = end;
                    cursor = end;
                    changed = true;
                } else {
                    cursor += 1;
                }
            }
            b'[' => {
                if let Some((end, replacement)) =
                    rewrite_array_destructuring_helper_writes(source, cursor, helpers)
                {
                    output.push_str(&source[last..cursor]);
                    output.push_str(replacement.as_str());
                    last = end;
                    cursor = end;
                    changed = true;
                } else {
                    cursor += 1;
                }
            }
            byte if is_identifier_start(byte) => {
                let start = cursor;
                cursor += 1;
                while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
                    cursor += 1;
                }
                if declaration_bindings.contains(&start) {
                    continue;
                }
                let identifier = &source[start..cursor];
                let Some(helper) = helpers
                    .iter()
                    .find(|binding| binding.as_str() == identifier)
                else {
                    continue;
                };
                if start
                    .checked_sub(1)
                    .and_then(|index| bytes.get(index))
                    .is_some_and(|byte| matches!(*byte, b'#' | b'.'))
                {
                    continue;
                }
                let equals = skip_ws(bytes, cursor);
                if let Some(operator) = update_operator_at(bytes, equals) {
                    output.push_str(&source[last..start]);
                    output.push_str(
                        runtime_helper_update_expression(helper, operator, UpdatePosition::Postfix)
                            .as_str(),
                    );
                    last = equals + 2;
                    cursor = equals + 2;
                    changed = true;
                    continue;
                }
                if bytes.get(equals) != Some(&b'=')
                    || bytes.get(equals + 1) == Some(&b'=')
                    || bytes.get(equals + 1) == Some(&b'>')
                {
                    continue;
                }
                let rhs_start = skip_ws(bytes, equals + 1);
                let rhs_end = find_assignment_rhs_end(source, rhs_start);
                let rhs = rewrite_runtime_helper_writes(&source[rhs_start..rhs_end], helpers);
                output.push_str(&source[last..start]);
                output.push_str(runtime_helper_setter_name(helper).as_str());
                output.push('(');
                output.push_str(rhs.as_str());
                output.push(')');
                last = rhs_end;
                cursor = rhs_end;
                changed = true;
            }
            _ => cursor += 1,
        }
    }
    if !changed {
        return source.to_string();
    }
    output.push_str(&source[last..]);
    output
}

#[derive(Clone, Copy)]
enum UpdatePosition {
    Prefix,
    Postfix,
}

fn runtime_helper_update_expression(
    binding: &BindingName,
    operator: UpdateOperator,
    position: UpdatePosition,
) -> String {
    let binding_name = binding.as_str();
    let setter = runtime_helper_setter_name(binding);
    match position {
        UpdatePosition::Prefix => format!(
            "(() => {{ let _$u = {binding_name}; let _$n = {operator}_$u; {setter}(_$u); return _$n; }})()",
            operator = operator.source()
        ),
        UpdatePosition::Postfix => format!(
            "(() => {{ let _$u = {binding_name}; let _$p = _$u{operator}; {setter}(_$u); return _$p; }})()",
            operator = operator.source()
        ),
    }
}

pub(crate) fn find_assignment_rhs_end(source: &str, mut cursor: usize) -> usize {
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
                cursor = skip_regex_literal(bytes, cursor);
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
            b')' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => return cursor,
            b']' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => return cursor,
            b'}' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => return cursor,
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
            b',' | b';' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => {
                return cursor;
            }
            _ => cursor += 1,
        }
    }
    cursor
}
