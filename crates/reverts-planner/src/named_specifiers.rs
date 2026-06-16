//! Named export / import specifier parsing extracted from `lib.rs`.
//!
//! Handles lightweight pattern matching against generated ESM
//! `export { ... } from "..."`, `export { ... }`, and `import { ... }`
//! statement texts. Used by the wiring step to recover re-export and
//! aliased-import shapes from emitted sources without needing the
//! ImportExport graph.

use crate::identifiers::is_identifier_like;
use crate::import_coalesce::{parse_named_import_clause, split_import_clause_and_specifier};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NamedReexportSpecifier {
    pub(crate) exported: String,
    pub(crate) is_aliased: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NamedImportSpecifier {
    pub(crate) local: String,
    pub(crate) is_aliased: bool,
}

pub(crate) fn source_statements(source: &str) -> Vec<&str> {
    source
        .split(';')
        .map(str::trim)
        .filter(|statement| !statement.is_empty())
        .collect()
}

pub(crate) fn named_reexport_specifiers(statement: &str) -> Option<Vec<NamedReexportSpecifier>> {
    let rest = statement.strip_prefix("export {")?;
    let (inner, after) = rest.split_once('}')?;
    if !after.trim_start().starts_with("from ") {
        return None;
    }
    let mut specifiers = Vec::new();
    for raw in inner.split(',') {
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with("type ") {
            return None;
        }
        let (imported, exported, is_aliased) = raw
            .split_once(" as ")
            .map_or((raw, raw, false), |(imported, exported)| {
                (imported.trim(), exported.trim(), true)
            });
        if !is_identifier_like(imported) || !is_identifier_like(exported) {
            return None;
        }
        specifiers.push(NamedReexportSpecifier {
            exported: exported.to_string(),
            is_aliased,
        });
    }
    Some(specifiers)
}

pub(crate) fn local_named_export_specifiers(
    statement: &str,
) -> Option<Vec<NamedReexportSpecifier>> {
    let rest = statement.strip_prefix("export {")?;
    let (inner, after) = rest.split_once('}')?;
    if !after.trim().is_empty() {
        return None;
    }
    parse_named_export_inner(inner)
}

pub(crate) fn parse_named_export_inner(inner: &str) -> Option<Vec<NamedReexportSpecifier>> {
    let mut specifiers = Vec::new();
    for raw in inner.split(',') {
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with("type ") {
            return None;
        }
        let (imported, exported, is_aliased) = raw
            .split_once(" as ")
            .map_or((raw, raw, false), |(imported, exported)| {
                (imported.trim(), exported.trim(), true)
            });
        if !is_identifier_like(imported) || !is_identifier_like(exported) {
            return None;
        }
        specifiers.push(NamedReexportSpecifier {
            exported: exported.to_string(),
            is_aliased,
        });
    }
    Some(specifiers)
}

pub(crate) fn named_import_specifiers(statement: &str) -> Option<Vec<NamedImportSpecifier>> {
    let rest = statement.strip_prefix("import ")?.trim();
    if rest.starts_with("type ") || rest.contains(" with ") || rest.contains(" assert ") {
        return None;
    }
    let (clause, _specifier) = split_import_clause_and_specifier(rest)?;
    if !clause.starts_with('{') {
        return None;
    }
    Some(
        parse_named_import_clause(clause)?
            .into_iter()
            .map(|(imported, local)| NamedImportSpecifier {
                is_aliased: imported != local,
                local,
            })
            .collect(),
    )
}
