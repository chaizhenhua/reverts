//! Folds `namespace.member` accesses into named-import substitutions.
//!
//! When a planned file imports a runtime namespace object whose
//! `Object.defineProperties` shape is fully known (paper #7 downstream
//! data), reads of `namespace.member` can be replaced with bare
//! `member` identifiers as long as:
//!
//! 1. The site is read-only (no `++`/`--`, no assignment-with-operator).
//! 2. Every member name is a real binding, not a JS keyword, and not
//!    the namespace itself.
//! 3. The replacement does not collide with a `reserved_bindings` name
//!    that the consumer already has (unless this rewrite is the one
//!    introducing it).
//!
//! If any access in a namespace is unsafe, that namespace is left
//! intact rather than partially rewritten — the planner does not split
//! a namespace export into "some inline / some preserved" because that
//! would split the binding identity across forms.
//!
//! Source surgery note: only the proven member-access spans are replaced; the
//! rest of the raw helper snippet is preserved byte-for-byte.

use std::collections::{BTreeMap, BTreeSet};

use reverts_graph::RevertsGraph;
use reverts_ir::BindingName;
use reverts_js::is_js_keyword;

use crate::byte_lexer::skip_ws;
use crate::{apply_text_edits, collect_member_access_only, previous_non_ws};

pub(crate) struct RuntimeNamespaceMemberAccessRewrite {
    pub(crate) source: String,
    pub(crate) imports_by_source: BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) dropped_namespaces_by_source: BTreeMap<u32, BTreeSet<BindingName>>,
}

pub(crate) fn rewrite_runtime_namespace_member_accesses(
    source: &str,
    runtime_import_groups: &BTreeMap<u32, BTreeSet<BindingName>>,
    graph: &RevertsGraph,
    reserved_bindings: &BTreeSet<BindingName>,
) -> Option<RuntimeNamespaceMemberAccessRewrite> {
    let mut edits = Vec::<(usize, usize, String)>::new();
    let mut imports_by_source = BTreeMap::<u32, BTreeSet<BindingName>>::new();
    let mut dropped_namespaces_by_source = BTreeMap::<u32, BTreeSet<BindingName>>::new();
    let mut introduced = BTreeSet::<BindingName>::new();

    for (source_file_id, imported_bindings) in runtime_import_groups {
        let Some(prelude) = graph.runtime_prelude(*source_file_id) else {
            continue;
        };
        for namespace_export in &prelude.namespace_exports {
            if !imported_bindings.contains(&namespace_export.namespace) {
                continue;
            }
            let properties = namespace_export
                .exports
                .iter()
                .map(|(key, target)| (key.clone(), target.as_str().to_string()))
                .collect::<Vec<_>>();
            let Some(access_sites) = collect_member_access_only(
                source,
                namespace_export.namespace.as_str(),
                (0, 0),
                &properties,
            ) else {
                continue;
            };
            if access_sites.is_empty() {
                continue;
            }

            let mut targets = BTreeSet::<BindingName>::new();
            let mut namespace_edits = Vec::<(usize, usize, String)>::new();
            let mut safe = true;
            for (start, end, key) in access_sites {
                if !runtime_namespace_member_access_site_is_read_only(source, start, end) {
                    safe = false;
                    break;
                }
                let Some(target) = namespace_export.exports.get(&key) else {
                    safe = false;
                    break;
                };
                if target == &namespace_export.namespace || is_js_keyword(target.as_str()) {
                    safe = false;
                    break;
                }
                if reserved_bindings.contains(target) && !introduced.contains(target) {
                    safe = false;
                    break;
                }
                targets.insert(target.clone());
                namespace_edits.push((start, end, target.as_str().to_string()));
            }
            if !safe || targets.is_empty() {
                continue;
            }

            introduced.extend(targets.iter().cloned());
            edits.extend(namespace_edits);
            imports_by_source
                .entry(*source_file_id)
                .or_default()
                .extend(targets);
            dropped_namespaces_by_source
                .entry(*source_file_id)
                .or_default()
                .insert(namespace_export.namespace.clone());
        }
    }

    if edits.is_empty() {
        return None;
    }
    edits.sort_by_key(|(start, _, _)| *start);
    if edits.windows(2).any(|window| window[0].1 > window[1].0) {
        return None;
    }
    Some(RuntimeNamespaceMemberAccessRewrite {
        source: apply_text_edits(source, &edits),
        imports_by_source,
        dropped_namespaces_by_source,
    })
}

pub(crate) fn runtime_namespace_member_access_site_is_read_only(
    source: &str,
    start: usize,
    end: usize,
) -> bool {
    let bytes = source.as_bytes();
    if let Some(before) = previous_non_ws(bytes, start)
        && before > 0
        && bytes
            .get(before - 1..=before)
            .is_some_and(|operator| operator == b"++" || operator == b"--")
    {
        return false;
    }
    let after = skip_ws(bytes, end);
    match bytes.get(after).copied() {
        Some(b'+') | Some(b'-') => {
            !matches!(bytes.get(after + 1), Some(b'+') | Some(b'-') | Some(b'='))
        }
        Some(b'=') => matches!(bytes.get(after + 1), Some(b'=')),
        Some(b'*') => {
            bytes.get(after + 1) != Some(&b'=')
                && !(bytes.get(after + 1) == Some(&b'*') && bytes.get(after + 2) == Some(&b'='))
        }
        Some(b'/' | b'%' | b'^') => bytes.get(after + 1) != Some(&b'='),
        Some(b'&') => {
            bytes.get(after + 1) != Some(&b'=')
                && !(bytes.get(after + 1) == Some(&b'&') && bytes.get(after + 2) == Some(&b'='))
        }
        Some(b'|') => {
            bytes.get(after + 1) != Some(&b'=')
                && !(bytes.get(after + 1) == Some(&b'|') && bytes.get(after + 2) == Some(&b'='))
        }
        Some(b'<') => !(bytes.get(after + 1) == Some(&b'<') && bytes.get(after + 2) == Some(&b'=')),
        Some(b'>') => {
            !(bytes.get(after + 1) == Some(&b'>')
                && (bytes.get(after + 2) == Some(&b'=')
                    || (bytes.get(after + 2) == Some(&b'>')
                        && bytes.get(after + 3) == Some(&b'='))))
        }
        Some(b'?') => !(bytes.get(after + 1) == Some(&b'?') && bytes.get(after + 2) == Some(&b'=')),
        _ => true,
    }
}
