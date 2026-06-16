//! Identifier read-fact analysis extracted from `lib.rs`.
//!
//! Wraps `reverts-js::collect_identifier_read_facts` with planner-facing
//! helpers used to rename, classify, or audit identifier occurrences inside
//! emitted source slices. All helpers here operate on raw `&str` source.

use std::collections::BTreeMap;

use reverts_ir::BindingName;
use reverts_js::{ParseGoal, collect_identifier_read_facts, is_ascii_identifier_continue};

use crate::byte_lexer::{find_matching_paren, skip_ws};
use crate::previous_non_ws;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IdentifierReadUsage {
    pub(crate) name: String,
    pub(crate) byte_start: usize,
    pub(crate) byte_end: usize,
    pub(crate) is_call_callee: bool,
}

pub(crate) fn identifier_read_facts_in_source(source: &str) -> Vec<IdentifierReadUsage> {
    try_identifier_read_facts_in_source(source).unwrap_or_default()
}

pub(crate) fn try_identifier_read_facts_in_source(
    source: &str,
) -> Option<Vec<IdentifierReadUsage>> {
    collect_identifier_read_facts(source, None, ParseGoal::TypeScript)
        .ok()
        .map(|facts| {
            facts
                .into_iter()
                .map(|fact| IdentifierReadUsage {
                    name: fact.name,
                    byte_start: fact.byte_start as usize,
                    byte_end: fact.byte_end as usize,
                    is_call_callee: fact.is_call_callee,
                })
                .collect()
        })
}

pub(crate) fn identifier_read_rename_sites_are_safe(source: &str, binding: &BindingName) -> bool {
    // When the generated source isn't parseable we can't enumerate reads,
    // so we must declare the rename unsafe rather than admitting it on an
    // empty fact set (which would silently miss usages).
    let Some(facts) = try_identifier_read_facts_in_source(source) else {
        return false;
    };
    facts
        .into_iter()
        .filter(|fact| fact.name == binding.as_str())
        .all(|fact| identifier_read_rename_site_is_safe(source, fact.byte_start, fact.byte_end))
}

pub(crate) fn identifier_read_rename_site_is_safe(source: &str, start: usize, end: usize) -> bool {
    let bytes = source.as_bytes();
    if previous_non_ws(bytes, start)
        .and_then(|index| bytes.get(index))
        .is_some_and(|byte| matches!(*byte, b'.' | b'#'))
    {
        return false;
    }
    let next = skip_ws(bytes, end);
    if bytes.get(next).is_some_and(|byte| *byte == b':') {
        return false;
    }
    let prev = previous_non_ws(bytes, start).and_then(|index| bytes.get(index).copied());
    let next_byte = bytes.get(next).copied();
    if matches!(prev, Some(b'{') | Some(b',')) && matches!(next_byte, Some(b'}') | Some(b',')) {
        return false;
    }
    true
}

pub(crate) fn rename_identifier_reads_in_source(
    source: &str,
    aliases: &BTreeMap<BindingName, BindingName>,
) -> String {
    if aliases.is_empty() {
        return source.to_string();
    }
    let mut edits = identifier_read_facts_in_source(source)
        .into_iter()
        .filter_map(|fact| {
            let alias = aliases.get(&BindingName::new(fact.name.as_str()))?;
            if !identifier_read_rename_site_is_safe(source, fact.byte_start, fact.byte_end) {
                return None;
            }
            Some((fact.byte_start, fact.byte_end, alias.as_str().to_string()))
        })
        .collect::<Vec<_>>();
    if edits.is_empty() {
        return source.to_string();
    }
    edits.sort_by(|left, right| right.0.cmp(&left.0));
    let mut rewritten = source.to_string();
    for (start, end, replacement) in edits {
        rewritten.replace_range(start..end, replacement.as_str());
    }
    rewritten
}

pub(crate) fn identifier_occurrence_is_value_reference(
    source: &str,
    start: usize,
    end: usize,
) -> bool {
    let bytes = source.as_bytes();
    if previous_non_ws(bytes, start)
        .and_then(|index| bytes.get(index))
        .is_some_and(|byte| *byte == b'#')
    {
        return false;
    }
    if previous_non_ws(bytes, start)
        .and_then(|index| bytes.get(index).map(|byte| (index, *byte)))
        .is_some_and(|(index, byte)| {
            byte == b'.' && index.checked_sub(1).and_then(|prev| bytes.get(prev)) != Some(&b'.')
        })
    {
        return false;
    }
    if identifier_is_declaration_name_after_keyword(source, start, "class")
        || identifier_is_declaration_name_after_keyword(source, start, "function")
    {
        return false;
    }

    let after = skip_ws(bytes, end);
    let before = previous_non_ws(bytes, start).and_then(|index| bytes.get(index));
    if bytes.get(after) == Some(&b'=') && bytes.get(after + 1) == Some(&b'>') {
        return false;
    }
    if bytes.get(after) == Some(&b':')
        && previous_non_ws(bytes, start)
            .and_then(|index| bytes.get(index))
            .is_some_and(|byte| matches!(*byte, b'{' | b',' | b'('))
    {
        return false;
    }

    if bytes.get(after) == Some(&b'=')
        && before.is_none_or(|byte| matches!(*byte, b'{' | b'}' | b';' | b',' | b'('))
    {
        return false;
    }

    if bytes.get(after) == Some(&b'(')
        && let Some(close) = find_matching_paren(source, after)
        && bytes.get(skip_ws(bytes, close + 1)) == Some(&b'{')
        && before.is_none_or(|byte| !matches!(*byte, b'.' | b')' | b']'))
    {
        return false;
    }

    true
}

pub(crate) fn identifier_is_declaration_name_after_keyword(
    source: &str,
    start: usize,
    keyword: &str,
) -> bool {
    let bytes = source.as_bytes();
    let Some(keyword_end) = previous_non_ws(bytes, start) else {
        return false;
    };
    let Some(keyword_start) = keyword_end
        .checked_add(1)
        .and_then(|end| end.checked_sub(keyword.len()))
    else {
        return false;
    };
    if bytes.get(keyword_start..keyword_end + 1) != Some(keyword.as_bytes()) {
        return false;
    }
    let before = keyword_start
        .checked_sub(1)
        .and_then(|index| bytes.get(index))
        .copied();
    let after = bytes.get(keyword_end + 1).copied();
    before.is_none_or(|byte| !is_ascii_identifier_continue(byte))
        && after.is_some_and(|byte| byte.is_ascii_whitespace())
}

pub(crate) fn control_flow_keyword_before_paren(source: &str, open_paren: usize) -> bool {
    match keyword_before_paren(source, open_paren) {
        Some("if" | "while" | "switch" | "for" | "catch" | "with") => true,
        Some("await") => for_keyword_before_await(source, open_paren),
        _ => false,
    }
}

pub(crate) fn keyword_before_paren(source: &str, open_paren: usize) -> Option<&str> {
    let bytes = source.as_bytes();
    let before = previous_non_ws(bytes, open_paren)?;
    // `previous_non_ws` walks raw bytes, so `before` can land inside a
    // multi-byte UTF-8 sequence. Identifier keywords are ASCII; bail
    // immediately when the preceding byte isn't ASCII so we never slice
    // across a char boundary.
    if !bytes[before].is_ascii() {
        return None;
    }
    let mut start = before;
    while start > 0 && is_ascii_identifier_continue(bytes[start - 1]) {
        start -= 1;
    }
    if start
        .checked_sub(1)
        .and_then(|index| bytes.get(index))
        .is_some_and(|byte| matches!(*byte, b'.' | b'#'))
    {
        return None;
    }
    Some(&source[start..=before])
}

pub(crate) fn for_keyword_before_await(source: &str, open_paren: usize) -> bool {
    let bytes = source.as_bytes();
    let await_end = match previous_non_ws(bytes, open_paren) {
        Some(index) => index,
        None => return false,
    };
    if !bytes[await_end].is_ascii() {
        return false;
    }
    let mut await_start = await_end;
    while await_start > 0 && is_ascii_identifier_continue(bytes[await_start - 1]) {
        await_start -= 1;
    }
    if &source[await_start..=await_end] != "await" {
        return false;
    }
    let Some(for_end) = previous_non_ws(bytes, await_start) else {
        return false;
    };
    if !bytes[for_end].is_ascii() {
        return false;
    }
    let mut for_start = for_end;
    while for_start > 0 && is_ascii_identifier_continue(bytes[for_start - 1]) {
        for_start -= 1;
    }
    &source[for_start..=for_end] == "for"
}

pub(crate) fn previous_token_is_keyword(source: &str, before: usize, keyword: &str) -> bool {
    let bytes = source.as_bytes();
    let Some(last) = previous_non_ws(bytes, before) else {
        return false;
    };
    if !is_ascii_identifier_continue(bytes[last]) {
        return false;
    }
    let mut start = last;
    while start > 0 && is_ascii_identifier_continue(bytes[start - 1]) {
        start -= 1;
    }
    &source[start..=last] == keyword
}

pub(crate) fn compact_js_source(source: &str) -> String {
    source
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}
