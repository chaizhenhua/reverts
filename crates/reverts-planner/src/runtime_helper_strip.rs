//! Surgical edits that prepare a runtime helper file for emission.
//!
//! When a binding migrates out of the runtime helpers module (Phase 10b/10c)
//! the helper file must drop the original declaration and any
//! `Object.defineProperties` namespace-export statement that targeted it.
//! These functions perform those line-precise removals while preserving the
//! rest of the helper body unchanged so downstream byte-walking passes (the
//! setter-call inliner, identifier reference scans) still see contiguous
//! recovered source.
//!
//! - `classify_migratable_var_declaration` decides whether a recovered
//!   `var/let/const X[ = INIT];` is safe to migrate; the planner uses its
//!   return value both to gate migration and to carry the initializer to
//!   the new owner module.
//! - `strip_runtime_var_declarations` erases the migrated `var X;` /
//!   `var X = LITERAL;` lines from the runtime body.
//! - `strip_runtime_snippet_sources` /
//!   `strip_runtime_namespace_export_sources` remove the recovered snippet
//!   or namespace-export statement that backed the migrated binding.
//! - `find_runtime_source_chunk` is the shared helper that locates a
//!   contiguous chunk whose boundaries align with newlines so the removal
//!   doesn't strand half a statement.

use std::collections::BTreeSet;

use reverts_graph::RuntimePrelude;
use reverts_ir::BindingName;

use crate::is_pure_initializer_expression;
use crate::statements::runtime_namespace_export_statement;

/// Decide whether a prelude var declaration is safe to migrate, and
/// extract its initializer expression when present.
///
/// Returns:
///   * `None` — declaration is not a single-binding `var/let/const X[ =
///     init];` statement, or the initializer is too complex to safely
///     copy to a writer module (calls, member access, identifier
///     references, etc.).
///   * `Some(None)` — bare `var X;` declaration with no initializer.
///   * `Some(Some(initializer))` — `var X = INIT;` where INIT is a
///     side-effect-free initializer that can be transplanted as-is.
pub(crate) fn classify_migratable_var_declaration<'a>(
    snippet: &'a str,
    binding: &str,
) -> Option<Option<&'a str>> {
    let trimmed = snippet.trim();
    for keyword in ["var", "let", "const"] {
        if let Some(rest) = trimmed.strip_prefix(keyword)
            && rest.starts_with(|c: char| c.is_ascii_whitespace())
        {
            let rest = rest.trim_start();
            let Some(rest) = rest.strip_suffix(';') else {
                continue;
            };
            let rest = rest.trim();
            // Bare `var X;`
            if rest == binding {
                return Some(None);
            }
            // `var X = INIT;`
            let mut splitter = rest.splitn(2, '=');
            let lhs = splitter.next()?.trim();
            let rhs = splitter.next()?.trim();
            if lhs != binding {
                continue;
            }
            if is_pure_initializer_expression(rhs) {
                return Some(Some(rhs));
            }
        }
    }
    None
}

/// Remove `var X;` or `var X = LITERAL;` declarations for each binding
/// in `bindings` from the runtime helper source. Used after the Phase
/// 10b/10c migration plan moves the declaration (and any literal
/// initializer) to a new owner module — the declaration is no longer
/// needed in the runtime, and leaving it would either create a
/// duplicate-declaration audit failure or shadow the re-exported
/// binding from the owner module.
pub(crate) fn strip_runtime_var_declarations<'a>(
    source: &str,
    bindings: impl IntoIterator<Item = &'a BindingName>,
) -> String {
    let drop_set: BTreeSet<&str> = bindings.into_iter().map(BindingName::as_str).collect();
    if drop_set.is_empty() {
        return source.to_string();
    }
    let mut out = String::with_capacity(source.len());
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
        let stripped = trimmed.trim();
        let matched = stripped
            .strip_prefix("var ")
            .and_then(|rest| rest.strip_suffix(';'))
            .map(|body| {
                // Strip everything past `=` for declarations with an
                // initializer; the migration's gate already verified the
                // initializer is a side-effect-free literal that the
                // writer carries verbatim, so the runtime line is safe
                // to drop entirely.
                body.split('=').next().unwrap_or(body).trim()
            })
            .is_some_and(|name| drop_set.contains(name));
        if matched {
            continue;
        }
        out.push_str(line);
    }
    out
}

pub(crate) fn strip_runtime_snippet_sources(
    source: &str,
    prelude: &RuntimePrelude,
    bindings: &BTreeSet<BindingName>,
) -> String {
    if bindings.is_empty() {
        return source.to_string();
    }
    let mut snippets = bindings
        .iter()
        .filter_map(|binding| prelude.snippets.get(binding))
        .collect::<Vec<_>>();
    snippets.sort_by(|left, right| right.byte_start.cmp(&left.byte_start));
    let mut stripped = source.to_string();
    for snippet in snippets {
        if let Some((start, end)) = find_runtime_source_chunk(&stripped, snippet.source.as_str()) {
            stripped.replace_range(start..end, "");
        }
    }
    stripped
}

pub(crate) fn strip_runtime_namespace_export_sources(
    source: &str,
    prelude: &RuntimePrelude,
    namespaces: &BTreeSet<BindingName>,
) -> String {
    if namespaces.is_empty() {
        return source.to_string();
    }
    let mut exports = prelude
        .namespace_exports
        .iter()
        .filter(|namespace_export| namespaces.contains(&namespace_export.namespace))
        .collect::<Vec<_>>();
    exports.sort_by(|left, right| right.byte_start.cmp(&left.byte_start));
    let mut stripped = source.to_string();
    for namespace_export in exports {
        let statement = runtime_namespace_export_statement(namespace_export);
        if let Some((start, end)) = find_runtime_source_chunk(&stripped, statement.as_str()) {
            stripped.replace_range(start..end, "");
        }
    }
    stripped
}

fn find_runtime_source_chunk(source: &str, chunk: &str) -> Option<(usize, usize)> {
    for (start, _) in source.match_indices(chunk) {
        let end = start + chunk.len();
        let before_ok = start == 0 || source.as_bytes().get(start - 1) == Some(&b'\n');
        let after_ok = end == source.len() || source.as_bytes().get(end) == Some(&b'\n');
        if !before_ok || !after_ok {
            continue;
        }
        let mut drop_start = start;
        let mut drop_end = end;
        if source.as_bytes().get(drop_end) == Some(&b'\n') {
            drop_end += 1;
        } else if drop_start > 0 && source.as_bytes().get(drop_start - 1) == Some(&b'\n') {
            drop_start -= 1;
        }
        return Some((drop_start, drop_end));
    }
    None
}
