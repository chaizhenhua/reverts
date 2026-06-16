//! Top-level import-declaration coalescing.
//!
//! When the planner stitches together a module body from many runtime
//! and source-derived snippets, the result often has multiple
//! `import … from 'X';` statements that target the same specifier.
//! Without merging, the emitted module looks redundantly verbose to
//! readers and adds parse overhead. `coalesce_top_level_import_declarations`
//! finds every group of mergeable declarations sharing a specifier and
//! collapses them into a single canonical `import default, { named } from 'X'`
//! line (with the rest erased and trivia compacted).
//!
//! Gating rules (encoded in `parse_mergeable_import_declaration`):
//!
//! - Skip `import type …`, namespace (`* as`), import-with-assertions /
//!   import-with-attributes — those are not safely mergeable here.
//! - Two declarations with conflicting default bindings only collapse
//!   their named-specifier portions; defaults stay as separate
//!   `import D from 'X';` statements.
//!
//! `import_statement_local_bindings` and `parse_runtime_prelude_direct_import`
//! also live here because they share the same import-line parser
//! plumbing — every consumer that needs to read information *out* of a
//! recovered import statement (rather than emit one) routes through these.

use std::collections::{BTreeMap, BTreeSet};

use reverts_ir::BindingName;

use crate::identifiers::is_identifier_like;
use crate::statements::{
    default_import_statement, default_named_import_alias_statement, named_import_alias_statement,
};
use crate::{
    RuntimePreludeDirectImport, RuntimePreludeDirectImportKind, apply_text_edits,
    top_level_statement_spans,
};

pub(crate) fn first_local_for_import<'a>(
    bindings: &'a BTreeSet<BindingName>,
    imports: &'a BTreeMap<BindingName, RuntimePreludeDirectImport>,
    target: &RuntimePreludeDirectImport,
) -> &'a BindingName {
    bindings
        .iter()
        .find(|binding| imports.get(*binding).is_some_and(|import| import == target))
        .unwrap_or_else(|| bindings.first().expect("non-empty import binding group"))
}

pub(crate) fn import_statement_local_bindings(source: &str) -> Option<BTreeSet<BindingName>> {
    let source = source.trim();
    let rest = source.strip_prefix("import ")?;
    if rest.starts_with("type ") {
        return None;
    }
    let rest = rest.strip_suffix(';')?.trim();
    if rest.contains(" with ") || rest.contains(" assert ") {
        return None;
    }
    let (clause, specifier) = split_import_clause_and_specifier(rest)?;
    if !is_bare_import_specifier(specifier) {
        return None;
    }
    let mut bindings = BTreeSet::<BindingName>::new();
    if let Some(namespace) = parse_namespace_import_clause(clause) {
        bindings.insert(BindingName::new(namespace));
        return Some(bindings);
    }
    let (default_part, rest) = split_default_import_clause(clause);
    if let Some(default_part) = default_part {
        bindings.insert(BindingName::new(default_part));
    }
    if let Some(rest) = rest {
        if let Some(namespace) = parse_namespace_import_clause(rest) {
            bindings.insert(BindingName::new(namespace));
        } else {
            for (_imported, local) in parse_named_import_clause(rest)? {
                bindings.insert(BindingName::new(local));
            }
        }
    }
    Some(bindings)
}

pub(crate) fn parse_runtime_prelude_direct_import(
    source: &str,
    binding: &BindingName,
) -> Option<RuntimePreludeDirectImport> {
    let source = source.trim();
    let rest = source.strip_prefix("import ")?;
    if rest.starts_with("type ") {
        return None;
    }
    let rest = rest.strip_suffix(';')?.trim();
    if rest.contains(" with ") || rest.contains(" assert ") {
        return None;
    }
    let (clause, specifier) = split_import_clause_and_specifier(rest)?;
    if !is_bare_import_specifier(specifier) {
        return None;
    }
    parse_import_clause_for_binding(clause, binding).map(|kind| RuntimePreludeDirectImport {
        source: specifier.to_string(),
        snippet_source: source.to_string(),
        snippet_byte_start: 0,
        kind,
    })
}

pub(crate) fn split_import_clause_and_specifier(rest: &str) -> Option<(&str, &str)> {
    for delimiter in [" from '", " from \""] {
        let Some((clause, tail)) = rest.rsplit_once(delimiter) else {
            continue;
        };
        let quote = delimiter.as_bytes().last().copied()? as char;
        let specifier = tail.strip_suffix(quote)?;
        return Some((clause.trim(), specifier));
    }
    None
}

fn parse_import_clause_for_binding(
    clause: &str,
    binding: &BindingName,
) -> Option<RuntimePreludeDirectImportKind> {
    let binding = binding.as_str();
    if let Some(namespace) = parse_namespace_import_clause(clause)
        && namespace == binding
    {
        return Some(RuntimePreludeDirectImportKind::Namespace);
    }

    let (default_part, rest) = split_default_import_clause(clause);
    if let Some(default_part) = default_part
        && default_part == binding
    {
        return Some(RuntimePreludeDirectImportKind::Default);
    }

    let rest = rest?;
    if let Some(namespace) = parse_namespace_import_clause(rest)
        && namespace == binding
    {
        return Some(RuntimePreludeDirectImportKind::Namespace);
    }

    for (imported, local) in parse_named_import_clause(rest)? {
        if local == binding {
            return Some(RuntimePreludeDirectImportKind::Named { imported });
        }
    }
    None
}

fn parse_namespace_import_clause(clause: &str) -> Option<&str> {
    let local = clause.trim().strip_prefix("* as ")?.trim();
    is_identifier_like(local).then_some(local)
}

fn split_default_import_clause(clause: &str) -> (Option<&str>, Option<&str>) {
    let clause = clause.trim();
    if clause.starts_with('{') || clause.starts_with("* as ") {
        return (None, Some(clause));
    }
    let (default_part, rest) = clause
        .split_once(',')
        .map_or((clause, None), |(default_part, rest)| {
            (default_part, Some(rest.trim()))
        });
    let default_part = default_part.trim();
    if is_identifier_like(default_part) {
        (Some(default_part), rest)
    } else {
        (None, rest)
    }
}

pub(crate) fn parse_named_import_clause(clause: &str) -> Option<Vec<(String, String)>> {
    let clause = clause.trim();
    let inner = clause.strip_prefix('{')?.strip_suffix('}')?.trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    let mut specifiers = Vec::new();
    for raw in inner.split(',') {
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with("type ") {
            return None;
        }
        let (imported, local) = raw
            .split_once(" as ")
            .map_or((raw, raw), |(imported, local)| {
                (imported.trim(), local.trim())
            });
        if !is_identifier_like(imported) || !is_identifier_like(local) {
            return None;
        }
        specifiers.push((imported.to_string(), local.to_string()));
    }
    Some(specifiers)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MergeableImportDeclaration {
    default_binding: Option<BindingName>,
    named_specifiers: BTreeSet<(String, BindingName)>,
    specifier: String,
}

pub(crate) fn coalesce_top_level_import_declarations(source: &str) -> String {
    let mut groups = BTreeMap::<String, Vec<(usize, usize, MergeableImportDeclaration)>>::new();
    for (start, end) in top_level_statement_spans(source) {
        let statement = &source[start..end];
        let Some(import) = parse_mergeable_import_declaration(statement) else {
            continue;
        };
        groups
            .entry(import.specifier.clone())
            .or_default()
            .push((start, end, import));
    }

    let mut edits = Vec::<(usize, usize, String)>::new();
    for (specifier, declarations) in groups {
        if declarations.len() < 2 {
            continue;
        }
        let default_bindings = declarations
            .iter()
            .filter_map(|(_, _, declaration)| declaration.default_binding.clone())
            .collect::<BTreeSet<_>>();
        let named_specifiers = declarations
            .iter()
            .flat_map(|(_, _, declaration)| declaration.named_specifiers.iter().cloned())
            .collect::<BTreeSet<_>>();
        if named_specifiers.is_empty() {
            continue;
        }
        if default_bindings.len() > 1 {
            let named_declarations = declarations
                .iter()
                .filter(|(_, _, declaration)| !declaration.named_specifiers.is_empty())
                .collect::<Vec<_>>();
            if named_declarations.len() < 2 {
                continue;
            }
            let replacement_candidates = named_declarations
                .iter()
                .copied()
                .filter(|(_, _, declaration)| declaration.default_binding.is_some())
                .collect::<Vec<_>>();
            let replacement_candidates = if replacement_candidates.is_empty() {
                named_declarations.clone()
            } else {
                replacement_candidates
            };
            let Some((replacement_start, replacement_end, replacement_declaration)) =
                replacement_candidates
                    .iter()
                    .min_by_key(|(start, _, _)| *start)
                    .map(|(start, end, declaration)| (*start, *end, declaration))
            else {
                continue;
            };
            let replacement =
                if let Some(default_binding) = replacement_declaration.default_binding.as_ref() {
                    default_named_import_alias_statement(
                        default_binding,
                        named_specifiers
                            .iter()
                            .map(|(imported, local)| (imported.as_str(), local)),
                        specifier.as_str(),
                    )
                } else {
                    named_import_alias_statement(
                        named_specifiers
                            .iter()
                            .map(|(imported, local)| (imported.as_str(), local)),
                        specifier.as_str(),
                    )
                };
            edits.push((replacement_start, replacement_end, replacement));
            for (start, end, declaration) in named_declarations {
                if *start == replacement_start {
                    continue;
                }
                let replacement = declaration
                    .default_binding
                    .as_ref()
                    .map_or_else(String::new, |binding| {
                        default_import_statement(binding, specifier.as_str())
                    });
                edits.push((*start, *end, replacement));
            }
            continue;
        }
        let mergeable_declarations = declarations
            .iter()
            .filter(|(_, _, declaration)| {
                !declaration.named_specifiers.is_empty()
                    || (default_bindings.len() == 1 && declaration.default_binding.is_some())
            })
            .collect::<Vec<_>>();
        if mergeable_declarations.len() < 2 {
            continue;
        }
        let Some((replacement_start, replacement_end, _)) = mergeable_declarations
            .iter()
            .min_by_key(|(start, _, _)| *start)
            .map(|(start, end, declaration)| (*start, *end, declaration))
        else {
            continue;
        };
        let replacement = if let Some(default_binding) = default_bindings.iter().next() {
            default_named_import_alias_statement(
                default_binding,
                named_specifiers
                    .iter()
                    .map(|(imported, local)| (imported.as_str(), local)),
                specifier.as_str(),
            )
        } else {
            named_import_alias_statement(
                named_specifiers
                    .iter()
                    .map(|(imported, local)| (imported.as_str(), local)),
                specifier.as_str(),
            )
        };
        edits.push((replacement_start, replacement_end, replacement));
        for (start, end, _) in mergeable_declarations {
            if *start == replacement_start {
                continue;
            }
            edits.push((*start, *end, String::new()));
        }
    }

    if edits.is_empty() {
        source.to_string()
    } else {
        compact_top_level_import_trivia(&apply_text_edits(source, &edits))
    }
}

fn parse_mergeable_import_declaration(source: &str) -> Option<MergeableImportDeclaration> {
    let source = source.trim();
    let rest = source.strip_prefix("import ")?;
    if rest.starts_with("type ") {
        return None;
    }
    let rest = rest.strip_suffix(';')?.trim();
    if rest.contains(" with ") || rest.contains(" assert ") {
        return None;
    }
    let (clause, specifier) = split_import_clause_and_specifier(rest)?;
    if parse_namespace_import_clause(clause).is_some() {
        return None;
    }
    let (default_part, rest) = split_default_import_clause(clause);
    if rest.is_some_and(|rest| parse_namespace_import_clause(rest).is_some()) {
        return None;
    }
    let default_binding = default_part.map(BindingName::new);
    let named_specifiers = match rest {
        Some(rest) => parse_named_import_clause(rest)?,
        None => Vec::new(),
    }
    .into_iter()
    .map(|(imported, local)| (imported, BindingName::new(local)))
    .collect::<BTreeSet<_>>();
    if default_binding.is_none() && named_specifiers.is_empty() {
        return None;
    }
    Some(MergeableImportDeclaration {
        default_binding,
        named_specifiers,
        specifier: specifier.to_string(),
    })
}

pub(crate) fn compact_top_level_import_trivia(source: &str) -> String {
    let spans = top_level_statement_spans(source);
    if spans.len() < 2 {
        return source.to_string();
    }
    let mut edits = Vec::<(usize, usize, String)>::new();
    for window in spans.windows(2) {
        let (previous_start, previous_end) = window[0];
        let (next_start, next_end) = window[1];
        let gap = &source[previous_end..next_start];
        if gap.is_empty() || !gap.chars().all(char::is_whitespace) {
            continue;
        }
        let previous = &source[previous_start..previous_end];
        let next = &source[next_start..next_end];
        let previous_is_import = is_static_import_declaration(previous);
        let next_is_import = is_static_import_declaration(next);
        let replacement =
            if next_is_import && (previous_is_import || previous.trim_end().ends_with(';')) {
                ""
            } else if gap.as_bytes().iter().filter(|byte| **byte == b'\n').count() > 1 {
                "\n"
            } else {
                continue;
            };
        edits.push((previous_end, next_start, replacement.to_string()));
    }
    if edits.is_empty() {
        source.to_string()
    } else {
        apply_text_edits(source, &edits)
    }
}

pub(crate) fn is_static_import_declaration(source: &str) -> bool {
    let source = source.trim();
    source.starts_with("import ") && source.ends_with(';')
}

pub(crate) fn is_bare_import_specifier(specifier: &str) -> bool {
    !specifier.is_empty() && !specifier.starts_with('.') && !specifier.starts_with('/')
}
