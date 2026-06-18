//! Top-level definition and implicit-global write scans extracted from
//! `lib.rs`.
//!
//! `top_level_definitions_in_source` enumerates the names declared at the
//! top level of a generated source slice via `function` / `class` /
//! `var` / `let` / `const`. `implicit_global_writes_in_source` complements
//! it by finding assignments to identifiers that were never declared
//! locally — those are implicit globals that must be promoted to explicit
//! `var` declarations during emission.

use std::collections::BTreeSet;

use reverts_ir::BindingName;
use reverts_js::{
    is_ascii_identifier_continue as is_identifier_continue,
    is_ascii_identifier_start as is_identifier_start, is_js_keyword, skip_block_comment,
    skip_line_comment,
};

use crate::byte_lexer::{
    find_matching_brace, find_matching_bracket, looks_like_regex_literal, skip_quoted,
    skip_regex_literal, skip_ws,
};
use crate::class_field_bindings::class_field_bindings_in_source;
use crate::destructure_writes::{
    array_destructuring_assignment_writes, array_pattern_binding_names,
    object_destructuring_assignment_writes, object_pattern_binding_names,
};
use crate::identifiers::{
    is_planner_synthetic_binding, keyword_at, parse_identifier,
    parse_identifier_after_function_keyword, parse_identifier_after_keyword,
};
use crate::runtime_helper_writes::{is_simple_update_target, update_operator_at};

pub(crate) fn top_level_definitions_in_source(source: &str) -> BTreeSet<BindingName> {
    let mut definitions = BTreeSet::new();
    let bytes = source.as_bytes();
    let mut cursor = 0;
    let mut depth = 0usize;
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
            b'{' => {
                depth += 1;
                cursor += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                cursor += 1;
            }
            _ if depth == 0 && keyword_at(source, cursor, "function") => {
                if let Some((binding, next)) =
                    parse_identifier_after_function_keyword(source, cursor)
                {
                    definitions.insert(BindingName::new(binding));
                    cursor = next;
                } else {
                    cursor += "function".len();
                }
            }
            _ if depth == 0 && keyword_at(source, cursor, "class") => {
                if let Some((binding, next)) =
                    parse_identifier_after_keyword(source, cursor, "class")
                {
                    definitions.insert(BindingName::new(binding));
                    cursor = next;
                } else {
                    cursor += "class".len();
                }
            }
            _ if depth == 0 && keyword_at(source, cursor, "var") => {
                cursor = collect_variable_declaration_definitions(
                    source,
                    cursor + "var".len(),
                    &mut definitions,
                );
            }
            _ if depth == 0 && keyword_at(source, cursor, "let") => {
                cursor = collect_variable_declaration_definitions(
                    source,
                    cursor + "let".len(),
                    &mut definitions,
                );
            }
            _ if depth == 0 && keyword_at(source, cursor, "const") => {
                cursor = collect_variable_declaration_definitions(
                    source,
                    cursor + "const".len(),
                    &mut definitions,
                );
            }
            _ => cursor += 1,
        }
    }
    definitions
}

pub(crate) fn implicit_global_declarations_for_module(
    source: &str,
    source_definitions: &BTreeSet<BindingName>,
    source_imports: &BTreeSet<BindingName>,
    planned_bindings: &BTreeSet<BindingName>,
) -> BTreeSet<BindingName> {
    let top_level_definitions = top_level_definitions_in_source(source);
    implicit_global_writes_in_source(source)
        .into_iter()
        .filter(|binding| !top_level_definitions.contains(binding))
        .filter(|binding| !source_definitions.contains(binding))
        .filter(|binding| !source_imports.contains(binding))
        .filter(|binding| !planned_bindings.contains(binding))
        .filter(|binding| !is_planner_synthetic_binding(binding.as_str()))
        .collect()
}

pub(crate) fn implicit_global_writes_in_source(source: &str) -> BTreeSet<BindingName> {
    let mut writes = BTreeSet::new();
    let declaration_bindings = variable_declaration_binding_starts(source);
    let class_field_bindings = class_field_bindings_in_source(source);
    let bytes = source.as_bytes();
    let mut cursor = 0;
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
            b'+' | b'-' if update_operator_at(bytes, cursor).is_some() => {
                let target_start = skip_ws(bytes, cursor + 2);
                if let Some((identifier, target_end)) = parse_identifier(source, target_start)
                    && is_simple_update_target(source, target_start, target_end)
                {
                    writes.insert(BindingName::new(identifier));
                }
                cursor += 1;
            }
            b'{' => {
                if let Some((end, bindings)) =
                    object_destructuring_assignment_writes(source, cursor)
                {
                    writes.extend(bindings);
                    cursor = end;
                } else {
                    cursor += 1;
                }
            }
            b'[' => {
                if let Some((end, bindings)) = array_destructuring_assignment_writes(source, cursor)
                {
                    writes.extend(bindings);
                    cursor = end;
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
                if !is_js_keyword(identifier)
                    && start
                        .checked_sub(1)
                        .and_then(|index| bytes.get(index))
                        .is_none_or(|byte| !matches!(*byte, b'.' | b'#'))
                    && !class_field_bindings.contains_key(&start)
                {
                    let after = skip_ws(bytes, cursor);
                    if (bytes.get(after) == Some(&b'=')
                        && bytes.get(after + 1) != Some(&b'=')
                        && bytes.get(after + 1) != Some(&b'>'))
                        || update_operator_at(bytes, after).is_some()
                    {
                        writes.insert(BindingName::new(identifier));
                    }
                }
            }
            _ => cursor += 1,
        }
    }
    writes
}

pub(crate) fn collect_variable_declaration_definitions(
    source: &str,
    cursor: usize,
    definitions: &mut BTreeSet<BindingName>,
) -> usize {
    let bytes = source.as_bytes();
    let mut cursor = collect_one_declarator_binding(source, cursor, definitions);

    let mut nested = 0usize;
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
            b'(' | b'[' | b'{' => {
                nested += 1;
                cursor += 1;
            }
            b')' | b']' | b'}' => {
                nested = nested.saturating_sub(1);
                cursor += 1;
            }
            b',' if nested == 0 => {
                cursor = collect_one_declarator_binding(source, cursor + 1, definitions);
            }
            b';' if nested == 0 => return cursor + 1,
            _ => cursor += 1,
        }
    }
    cursor
}

/// Record the binding(s) of one declarator at `cursor` — a plain identifier
/// (`var x`) or a destructuring pattern (`var { key: local }`, `var [a, b]`) —
/// and return the byte just past it. Renamed object patterns bind the alias,
/// which is exactly the name later passes and importers reference, so missing
/// them here would make a real binding look undeclared (and its re-export get
/// pruned).
fn collect_one_declarator_binding(
    source: &str,
    cursor: usize,
    definitions: &mut BTreeSet<BindingName>,
) -> usize {
    let bytes = source.as_bytes();
    let cursor = skip_ws(bytes, cursor);
    match bytes.get(cursor) {
        Some(b'{') => {
            if let Some(end) = find_matching_brace(source, cursor) {
                for binding in object_pattern_binding_names(&source[cursor + 1..end]) {
                    definitions.insert(binding);
                }
                return end + 1;
            }
            cursor
        }
        Some(b'[') => {
            if let Some(end) = find_matching_bracket(source, cursor) {
                for binding in array_pattern_binding_names(&source[cursor + 1..end]) {
                    definitions.insert(binding);
                }
                return end + 1;
            }
            cursor
        }
        _ => {
            if let Some((binding, next)) = parse_identifier(source, cursor) {
                definitions.insert(BindingName::new(binding));
                next
            } else {
                cursor
            }
        }
    }
}

pub(crate) fn variable_declaration_binding_starts(source: &str) -> BTreeSet<usize> {
    let mut starts = BTreeSet::new();
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
            _ if keyword_at(source, cursor, "var") => {
                cursor = collect_variable_declaration_binding_starts(
                    source,
                    cursor + "var".len(),
                    &mut starts,
                );
            }
            _ if keyword_at(source, cursor, "let") => {
                cursor = collect_variable_declaration_binding_starts(
                    source,
                    cursor + "let".len(),
                    &mut starts,
                );
            }
            _ if keyword_at(source, cursor, "const") => {
                cursor = collect_variable_declaration_binding_starts(
                    source,
                    cursor + "const".len(),
                    &mut starts,
                );
            }
            _ => cursor += 1,
        }
    }
    starts
}

pub(crate) fn collect_variable_declaration_binding_starts(
    source: &str,
    mut cursor: usize,
    starts: &mut BTreeSet<usize>,
) -> usize {
    let bytes = source.as_bytes();
    cursor = skip_ws(bytes, cursor);
    if parse_identifier(source, cursor).is_some() {
        starts.insert(cursor);
    }

    let mut nested = 0usize;
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
            b'(' | b'[' | b'{' => {
                nested += 1;
                cursor += 1;
            }
            b')' if nested == 0 => return cursor + 1,
            b')' | b']' | b'}' => {
                nested = nested.saturating_sub(1);
                cursor += 1;
            }
            b',' if nested == 0 => {
                cursor = skip_ws(bytes, cursor + 1);
                if parse_identifier(source, cursor).is_some() {
                    starts.insert(cursor);
                }
            }
            b';' if nested == 0 => return cursor + 1,
            _ => cursor += 1,
        }
    }
    cursor
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definitions(source: &str) -> BTreeSet<String> {
        top_level_definitions_in_source(source)
            .into_iter()
            .map(|binding| binding.as_str().to_string())
            .collect()
    }

    #[test]
    fn object_destructuring_declaration_binds_renamed_locals() {
        // The esbuild interop preamble in a generated runtime-helper barrel:
        // each alias is a real top-level binding that other modules import, so
        // it must be recognized or its re-export gets pruned (ERR at runtime).
        let defs = definitions(
            "var { getPrototypeOf: Vz9, defineProperty: CF1, getOwnPropertyNames: Hz9 } = Object;",
        );
        assert!(defs.contains("Vz9"), "{defs:?}");
        assert!(defs.contains("CF1"), "{defs:?}");
        assert!(defs.contains("Hz9"), "{defs:?}");
        // The object keys themselves are not bindings.
        assert!(!defs.contains("getPrototypeOf"));
        assert!(!defs.contains("defineProperty"));
    }

    #[test]
    fn shorthand_rest_and_array_patterns_bind_their_locals() {
        let defs = definitions("const { a, b: c, ...rest } = obj; let [x, , y] = pair;");
        for expected in ["a", "c", "rest", "x", "y"] {
            assert!(defs.contains(expected), "missing {expected}: {defs:?}");
        }
        assert!(
            !defs.contains("b"),
            "renamed key is not a binding: {defs:?}"
        );
    }

    #[test]
    fn simple_and_mixed_declarations_still_resolve() {
        let defs = definitions("var Kz9 = Object.create, Dz9 = Object.prototype.hasOwnProperty;");
        assert!(defs.contains("Kz9"));
        assert!(defs.contains("Dz9"));

        let mixed = definitions("const first = 1, { second } = obj, [third] = arr;");
        for expected in ["first", "second", "third"] {
            assert!(mixed.contains(expected), "missing {expected}: {mixed:?}");
        }
    }

    #[test]
    fn object_literal_rhs_is_not_mistaken_for_bindings() {
        // The RHS object literal's keys must not leak into the binding set.
        let defs = definitions("const { picked } = { picked: 1, ignored: 2 };");
        assert!(defs.contains("picked"));
        assert!(!defs.contains("ignored"), "{defs:?}");
    }
}
