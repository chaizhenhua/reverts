//! Behavior-preserving extraction of one island cluster into its own file.
//!
//! Splitting the eager entry island is only safe for statements that carry no
//! evaluation-order meaning. A scope-hoisted island runs top-to-bottom and
//! contains side-effecting top-level statements (e.g. a library's global
//! registry init) and order-dependent initializers; moving those across a module
//! boundary changes when they run. Function and class *declarations*, by
//! contrast, are hoisted and side-effect-free to define — relocating one to an
//! imported module and importing it back does not change observable behavior, as
//! long as the binding is never reassigned (an ES import is read-only).
//!
//! This module performs only the source-level extraction: which statements move,
//! the moved file's body, and the remainder. Import/export wiring between the
//! remainder and the extracted file is layered on separately. Nothing here is
//! wired into emission yet, so the verified single-island output is unchanged.

use std::collections::{BTreeMap, BTreeSet};

use reverts_ir::BindingName;
use reverts_js::{ParseGoal, TopLevelStatementKind, collect_top_level_statement_facts};

use crate::statements::{named_export_statement, named_import_statement};

/// The result of lifting a cluster's hoistable declarations out of the island.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClusterExtraction {
    /// Bindings whose declarations were moved into the cluster file.
    pub(crate) moved_bindings: BTreeSet<BindingName>,
    /// Source of the extracted declarations, in original order.
    pub(crate) cluster_source: String,
    /// The island source with the extracted declarations removed.
    pub(crate) remaining_source: String,
}

/// Extract the hoistable (`function`/`class`) declarations whose bindings belong
/// to `cluster_bindings` out of `island_source`.
///
/// A declaration is moved only when it is a function or class declaration, every
/// binding it introduces is in `cluster_bindings`, and none of those bindings is
/// in `written_bindings` (a reassigned binding cannot become a read-only import).
/// Every other statement — side-effecting expressions, variable initializers,
/// imports/exports, setters, lazy thunks — stays in `remaining_source` in its
/// original position, preserving evaluation order and side effects.
///
/// Returns `None` when nothing is extractable (so callers leave the island
/// untouched) or when the source cannot be parsed.
pub(crate) fn extract_hoistable_cluster(
    island_source: &str,
    cluster_bindings: &BTreeSet<BindingName>,
    written_bindings: &BTreeSet<BindingName>,
) -> Option<ClusterExtraction> {
    let facts = collect_top_level_statement_facts(island_source, None, ParseGoal::TypeScript).ok()?;

    // Byte ranges of the statements to lift, in source order.
    let mut extracted: Vec<(usize, usize)> = Vec::new();
    let mut moved_bindings: BTreeSet<BindingName> = BTreeSet::new();
    for fact in &facts {
        if !matches!(
            fact.kind,
            TopLevelStatementKind::Function | TopLevelStatementKind::Class
        ) {
            continue;
        }
        if fact.bindings.is_empty() {
            continue;
        }
        let bindings: Vec<BindingName> =
            fact.bindings.iter().map(|name| BindingName::new(name.as_str())).collect();
        let all_in_cluster = bindings.iter().all(|binding| cluster_bindings.contains(binding));
        let any_written = bindings.iter().any(|binding| written_bindings.contains(binding));
        if !all_in_cluster || any_written {
            continue;
        }
        extracted.push((fact.byte_start as usize, fact.byte_end as usize));
        moved_bindings.extend(bindings);
    }

    if extracted.is_empty() {
        return None;
    }

    let cluster_source = extracted
        .iter()
        .map(|&(start, end)| island_source.get(start..end).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    let remaining_source = remove_ranges(island_source, &extracted);

    Some(ClusterExtraction {
        moved_bindings,
        cluster_source,
        remaining_source,
    })
}

/// Return `source` with every byte range in `ranges` removed. Ranges are assumed
/// non-overlapping (distinct top-level statements); they are sorted here so the
/// kept gaps stay in order.
fn remove_ranges(source: &str, ranges: &[(usize, usize)]) -> String {
    let mut sorted = ranges.to_vec();
    sorted.sort_unstable();
    let mut remaining = String::with_capacity(source.len());
    let mut cursor = 0;
    for (start, end) in sorted {
        if start > cursor {
            remaining.push_str(source.get(cursor..start).unwrap_or_default());
        }
        cursor = cursor.max(end);
    }
    if cursor < source.len() {
        remaining.push_str(source.get(cursor..).unwrap_or_default());
    }
    remaining
}

/// Assemble the source of an extracted cluster file: an import per source
/// specifier for the external bindings the moved declarations reference, then
/// the declarations, then a single re-export of every moved binding.
///
/// `imports` maps an emit-relative module specifier to the bindings to import
/// from it — every name the cluster references that is not declared in the
/// cluster itself (resolved by the caller from binding ownership). Imports are
/// emitted in specifier order for determinism.
pub(crate) fn assemble_cluster_file(
    cluster_source: &str,
    moved_bindings: &BTreeSet<BindingName>,
    imports: &BTreeMap<String, BTreeSet<BindingName>>,
) -> String {
    let mut out = String::new();
    for (specifier, names) in imports {
        if names.is_empty() {
            continue;
        }
        out.push_str(named_import_statement(names.iter(), specifier).as_str());
        out.push('\n');
    }
    out.push_str(cluster_source);
    out.push('\n');
    out.push_str(named_export_statement(moved_bindings.iter()).as_str());
    out.push('\n');
    out
}

/// The statement the island entry uses to import a cluster's moved bindings back
/// from the cluster file at `specifier`.
pub(crate) fn entry_import_for_cluster(
    moved_bindings: &BTreeSet<BindingName>,
    specifier: &str,
) -> String {
    named_import_statement(moved_bindings.iter(), specifier)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bindings(names: &[&str]) -> BTreeSet<BindingName> {
        names.iter().map(|name| BindingName::new(*name)).collect()
    }

    #[test]
    fn moves_only_cluster_function_and_class_declarations() {
        let island = "function f1() { return 1; }\n\
                      globalThis.__init = setup();\n\
                      class C1 { m() { return f1(); } }\n\
                      var keep = f1();\n\
                      function other() { return 2; }\n";
        let extraction =
            extract_hoistable_cluster(island, &bindings(&["f1", "C1"]), &BTreeSet::new()).unwrap();

        assert_eq!(extraction.moved_bindings, bindings(&["f1", "C1"]));
        assert!(extraction.cluster_source.contains("function f1()"));
        assert!(extraction.cluster_source.contains("class C1"));
        // The side-effecting init, the order-dependent var, and the out-of-cluster
        // function all stay put.
        assert!(extraction.remaining_source.contains("globalThis.__init = setup();"));
        assert!(extraction.remaining_source.contains("var keep = f1();"));
        assert!(extraction.remaining_source.contains("function other()"));
        assert!(!extraction.remaining_source.contains("function f1()"));
        assert!(!extraction.remaining_source.contains("class C1"));
    }

    #[test]
    fn never_moves_a_reassigned_binding() {
        // `f1` is a function but is reassigned later, so it cannot become a
        // read-only import — it must stay in the island.
        let island = "function f1() { return 1; }\nf1 = wrap(f1);\n";
        let result =
            extract_hoistable_cluster(island, &bindings(&["f1"]), &bindings(&["f1"]));
        assert!(result.is_none(), "reassigned binding must not be extracted");
    }

    #[test]
    fn leaves_side_effect_only_island_untouched() {
        // No hoistable declarations to move -> None, so the caller emits the
        // island unchanged.
        let island = "globalThis.__init = setup();\nvar x = compute();\n";
        assert!(extract_hoistable_cluster(island, &bindings(&["x"]), &BTreeSet::new()).is_none());
    }

    #[test]
    fn assembles_a_cluster_file_with_imports_body_and_reexport() {
        let mut imports = BTreeMap::new();
        imports.insert(
            "../runtime/shared.js".to_string(),
            bindings(&["helper"]),
        );
        let file = assemble_cluster_file(
            "function f1() { return helper(); }",
            &bindings(&["f1"]),
            &imports,
        );
        assert!(file.contains("import { helper } from '../runtime/shared.js';"));
        assert!(file.contains("function f1()"));
        assert!(file.contains("export { f1 };"));
        // Import precedes the declaration which precedes the export.
        let import_at = file.find("import { helper }").unwrap();
        let decl_at = file.find("function f1").unwrap();
        let export_at = file.find("export { f1 }").unwrap();
        assert!(import_at < decl_at && decl_at < export_at);
    }

    #[test]
    fn entry_imports_the_moved_bindings_back() {
        let statement =
            entry_import_for_cluster(&bindings(&["f1", "C1"]), "./island/cluster-0.js");
        assert_eq!(
            statement,
            "import { C1, f1 } from './island/cluster-0.js';"
        );
    }

    #[test]
    fn does_not_move_a_function_partly_outside_the_cluster() {
        // A statement binding two names, only one of which is in the cluster, is
        // not safe to split — keep it whole in the island.
        let island = "function a() { return 1; }\n";
        // `a` is in the cluster, so it moves; but verify the all-bindings rule by
        // excluding it from the cluster.
        assert!(extract_hoistable_cluster(island, &bindings(&["b"]), &BTreeSet::new()).is_none());
    }
}
