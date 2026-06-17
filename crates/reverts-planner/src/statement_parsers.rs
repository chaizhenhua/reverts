//! Reverse-parsers for planner-generated body lines.
//!
//! Three of the planner's coalescing passes (named-import merge,
//! default+named-import merge, named-export merge) need to recognise
//! their own previously-emitted lines so they can fold duplicates. The
//! formatters in `statements.rs` produce a single canonical shape per
//! concept; the parsers below recognise exactly that shape and nothing
//! else. They intentionally refuse anything ambiguous (extra
//! whitespace, `as` aliases, `from` re-exports inside an export clause)
//! so user-written or compiler-emitted code that happens to look
//! similar does not get rewritten.
//!
//! The `coalesce_consecutive_uninitialized_var_declarations` pass lives
//! here too because it shares the same "recognise a planner-emitted
//! line, then merge" pattern — it walks body lines, identifies
//! single-binding `var X;` declarations, and rewrites consecutive runs
//! into `var A, B, C;`.

use std::collections::BTreeSet;

use reverts_ir::BindingName;
use reverts_js::{
    is_ascii_identifier_continue as is_identifier_continue,
    is_ascii_identifier_start as is_identifier_start, is_js_keyword,
};

use crate::identifiers::{is_identifier_like, parse_identifier};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NamedImportSpecifier {
    pub(crate) imported: BindingName,
    pub(crate) local: BindingName,
}

pub(crate) fn parse_generated_named_import_statement(
    source: &str,
) -> Option<(BTreeSet<BindingName>, String)> {
    let (specifiers, specifier) = parse_generated_named_import_specifiers(source)?;
    let bindings = specifiers
        .into_iter()
        .map(|specifier| (specifier.imported == specifier.local).then_some(specifier.imported))
        .collect::<Option<BTreeSet<_>>>()?;
    if bindings.is_empty() {
        None
    } else {
        Some((bindings, specifier))
    }
}

pub(crate) fn parse_generated_named_import_specifiers(
    source: &str,
) -> Option<(Vec<NamedImportSpecifier>, String)> {
    let source = source.trim();
    let rest = source.strip_prefix("import { ")?;
    let (names, rest) = rest.split_once(" } from '")?;
    let specifier = rest.strip_suffix("';")?;
    if names.trim().is_empty() {
        return None;
    }
    let specifiers = names
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(parse_named_import_specifier)
        .collect::<Option<Vec<_>>>()?;
    if specifiers.is_empty() {
        None
    } else {
        Some((specifiers, specifier.to_string()))
    }
}

fn parse_named_import_specifier(source: &str) -> Option<NamedImportSpecifier> {
    let (imported, local) = source
        .split_once(" as ")
        .map_or((source, source), |(imported, local)| {
            (imported.trim(), local.trim())
        });
    Some(NamedImportSpecifier {
        imported: parse_binding_name(imported)?,
        local: parse_binding_name(local)?,
    })
}

fn parse_binding_name(source: &str) -> Option<BindingName> {
    let (identifier, end) = parse_identifier(source, 0)?;
    (end == source.len() && !is_js_keyword(identifier)).then(|| BindingName::new(identifier))
}

pub(crate) fn parse_generated_default_import_statement(
    source: &str,
) -> Option<(BindingName, String)> {
    let source = source.trim();
    let rest = source.strip_prefix("import ")?;
    let (binding, rest) = rest.split_once(" from '")?;
    let specifier = rest.strip_suffix("';")?;
    let binding = binding.trim();
    is_identifier_like(binding).then(|| (BindingName::new(binding), specifier.to_string()))
}

pub(crate) fn parse_generated_named_reexport_statement(
    source: &str,
) -> Option<(BTreeSet<BindingName>, String)> {
    let source = source.trim();
    let rest = source.strip_prefix("export { ")?;
    let (names, rest) = rest.split_once(" } from '")?;
    let specifier = rest.strip_suffix("';")?;
    if names.trim().is_empty() {
        return None;
    }
    let bindings = names
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(BindingName::new)
        .collect::<BTreeSet<_>>();
    if bindings.is_empty() {
        None
    } else {
        Some((bindings, specifier.to_string()))
    }
}

pub(crate) fn parse_generated_named_export_statement(
    source: &str,
) -> Option<BTreeSet<BindingName>> {
    let source = source.trim();
    let names = source.strip_prefix("export { ")?.strip_suffix(" };")?;
    if names.trim().is_empty() || names.contains(" from ") || names.contains(" as ") {
        return None;
    }
    let mut bindings = BTreeSet::<BindingName>::new();
    for name in names.split(',').map(str::trim) {
        if name.is_empty() {
            return None;
        }
        let (identifier, end) = parse_identifier(name, 0)?;
        if end != name.len() || is_js_keyword(identifier) {
            return None;
        }
        bindings.insert(BindingName::new(identifier));
    }
    (!bindings.is_empty()).then_some(bindings)
}

pub(crate) fn coalesce_consecutive_uninitialized_var_declarations(source: &str) -> String {
    let mut output = Vec::<String>::new();
    let mut pending = Vec::<String>::new();
    for line in source.lines() {
        if let Some(binding) = parse_single_uninitialized_var_line(line) {
            pending.push(binding.to_string());
            continue;
        }
        flush_uninitialized_var_run(&mut output, &mut pending);
        output.push(line.to_string());
    }
    flush_uninitialized_var_run(&mut output, &mut pending);
    if source.ends_with('\n') {
        format!("{}\n", output.join("\n"))
    } else {
        output.join("\n")
    }
}

fn flush_uninitialized_var_run(output: &mut Vec<String>, pending: &mut Vec<String>) {
    if pending.is_empty() {
        return;
    }
    if pending.len() == 1 {
        output.push(format!("var {};", pending[0]));
    } else {
        output.push(format!("var {};", pending.join(", ")));
    }
    pending.clear();
}

fn parse_single_uninitialized_var_line(line: &str) -> Option<&str> {
    let line = line.trim();
    let rest = line.strip_prefix("var ")?;
    let name = rest.strip_suffix(';')?;
    let bytes = name.as_bytes();
    if bytes.is_empty() || !is_identifier_start(bytes[0]) {
        return None;
    }
    if bytes[1..].iter().all(|byte| is_identifier_continue(*byte)) {
        Some(name)
    } else {
        None
    }
}
