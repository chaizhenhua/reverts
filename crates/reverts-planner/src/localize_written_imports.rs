//! Correctness pass: a module must not import a binding it reassigns.
//!
//! ESM `import` bindings are immutable; `X = …` against an `import { X }` throws
//! `TypeError: Assignment to constant variable` at module load. esbuild
//! scope-hoists a shared mutable `var` into one module's slice while the WRITE is
//! sliced into another module — e.g. a zod-style private-field WeakMap declared
//! `var bO` in a lazy module but initialized `bO = new WeakMap()` in the eager
//! entrypoint. Import wiring then binds the writer to that name via an import,
//! producing the illegal reassignment.
//!
//! This pass makes the WRITER own the binding instead: for every emitted file,
//! any imported name the file also writes is dropped from its import clause and
//! declared as a local `var`. Other readers keep importing the name from its
//! original declarer; only the writer gets its own mutable copy (a `var` hoists,
//! so call-time reads after the eager init observe the written value). It runs
//! after every import-completion pass so it sees the final import set, removes an
//! import + adds a hoisted declaration, and creates no new module-to-module edge.
//!
//! Statement boundaries come from the parser (`top_level_statement_spans`), not a
//! line split: the lowered island body concatenates many imports onto one line.

use std::collections::BTreeSet;

use reverts_ir::BindingName;

use crate::statement_parsers::{NamedImportSpecifier, parse_generated_named_import_specifiers};
use crate::top_level_definitions::implicit_global_writes_in_source;
use crate::{
    EmitPlan, apply_text_edits, top_level_definitions_in_source, top_level_statement_spans,
};

pub(crate) fn localize_written_imports(plan: &mut EmitPlan) {
    for file in &mut plan.files {
        let body = file.body.join("\n");
        let writes = implicit_global_writes_in_source(body.as_str());
        if writes.is_empty() {
            continue;
        }

        let mut edits = Vec::<(usize, usize, String)>::new();
        let mut localized = BTreeSet::<BindingName>::new();
        let mut first_non_import: Option<usize> = None;
        for (start, end) in top_level_statement_spans(body.as_str()) {
            let statement = body[start..end].trim();
            let Some((specifiers, specifier)) = parse_generated_named_import_specifiers(statement)
            else {
                if first_non_import.is_none() && !statement.is_empty() {
                    first_non_import = Some(start);
                }
                continue;
            };
            let (kept, dropped): (Vec<_>, Vec<_>) = specifiers
                .into_iter()
                .partition(|spec| !writes.contains(&spec.local));
            if dropped.is_empty() {
                continue;
            }
            for spec in &dropped {
                localized.insert(spec.local.clone());
            }
            let replacement = if kept.is_empty() {
                String::new()
            } else {
                format!("import {{ {} }} from '{specifier}';", render_clause(&kept))
            };
            edits.push((start, end, replacement));
        }
        if edits.is_empty() {
            continue;
        }

        // Declare the re-homed bindings the file does not otherwise declare,
        // hoisted just before the first non-import statement (so a leading
        // directive / comment prologue and the surviving imports stay first).
        let declared = top_level_definitions_in_source(body.as_str());
        let to_declare = localized
            .iter()
            .filter(|binding| !declared.contains(*binding))
            .map(BindingName::as_str)
            .collect::<Vec<_>>();
        if !to_declare.is_empty() {
            let at = first_non_import.unwrap_or(body.len());
            edits.push((at, at, format!("var {};\n", to_declare.join(", "))));
        }
        edits.sort_by_key(|(start, _, _)| *start);
        file.body = vec![apply_text_edits(body.as_str(), &edits)];
    }
}

fn render_clause(specifiers: &[NamedImportSpecifier]) -> String {
    specifiers
        .iter()
        .map(|spec| {
            if spec.imported.as_str() == spec.local.as_str() {
                spec.local.as_str().to_string()
            } else {
                format!("{} as {}", spec.imported.as_str(), spec.local.as_str())
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::PlannedFile;

    fn body_of<'a>(plan: &'a EmitPlan, path: &str) -> String {
        plan.files
            .iter()
            .find(|file| file.path == path)
            .map(|file| file.body.join("\n"))
            .expect("file present")
    }

    #[test]
    fn writer_owns_an_imported_binding_it_reassigns() {
        let mut plan = EmitPlan::default();
        let mut entry = PlannedFile::new("modules/entrypoint.ts");
        // Imports concatenated on one line, like the lowered island body.
        entry.push_source(
            "import { bO } from './496.js';import { rT } from './x.js';\nbO = new WeakMap();\nfunction m() { return iA(rT, bO); }",
        );
        plan.push_file(entry);

        localize_written_imports(&mut plan);

        let entry = body_of(&plan, "modules/entrypoint.ts");
        assert!(!entry.contains("{ bO }"), "bO import dropped: {entry}");
        assert!(
            entry.contains("import { rT } from './x.js';"),
            "other import kept: {entry}"
        );
        assert!(entry.contains("var bO;"), "bO declared locally: {entry}");
    }

    #[test]
    fn keeps_other_specifiers_when_one_is_written() {
        let mut plan = EmitPlan::default();
        let mut entry = PlannedFile::new("modules/entrypoint.ts");
        entry.push_source("import { a, bO, c } from './dep.js';\nbO = 1;\nlet z = a + c;");
        plan.push_file(entry);

        localize_written_imports(&mut plan);

        let entry = body_of(&plan, "modules/entrypoint.ts");
        assert!(
            entry.contains("import { a, c } from './dep.js';"),
            "{entry}"
        );
        assert!(entry.contains("var bO;"), "{entry}");
    }

    #[test]
    fn read_only_imports_are_untouched() {
        let mut plan = EmitPlan::default();
        let mut entry = PlannedFile::new("modules/entrypoint.ts");
        entry.push_source("import { a, b } from './dep.js';\nlet z = a + b;");
        plan.push_file(entry);

        localize_written_imports(&mut plan);

        assert!(
            body_of(&plan, "modules/entrypoint.ts").contains("import { a, b } from './dep.js';")
        );
    }

    #[test]
    fn does_not_redeclare_a_binding_already_local() {
        let mut plan = EmitPlan::default();
        let mut entry = PlannedFile::new("modules/entrypoint.ts");
        entry.push_source("import { bO } from './dep.js';\nvar bO;\nbO = 1;");
        plan.push_file(entry);

        localize_written_imports(&mut plan);

        let entry = body_of(&plan, "modules/entrypoint.ts");
        assert_eq!(
            entry.matches("var bO").count(),
            1,
            "no duplicate var: {entry}"
        );
        assert!(!entry.contains("import { bO }"), "{entry}");
    }
}
