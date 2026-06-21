//! Completion pass: import a referenced runtime helper from the helpers file
//! that defines it, even across source files.
//!
//! esbuild's bundle-wide helpers (notably the `__esm` lazy initializer `st`) are
//! shared by every module in the original single scope, but the planner emits
//! them in ONE source file's helpers module (`runtime/source-N-helpers.ts`) and
//! wires the `import { st }` only to modules of that same source file. A module
//! in a DIFFERENT source file (whose source file has no helpers module of its
//! own) that uses the same helper ends up referencing `st` with no import → a
//! `ReferenceError: st is not defined` at load.
//!
//! [`complete_referenced_module_imports`] only AUGMENTS an existing import (it
//! must never add a module edge, to stay cycle-safe). Helpers are emitted as
//! hoisted `function` declarations (see [`crate::helper_hoisting`]), which are
//! live across import cycles, so adding a NEW import edge to a helpers file is
//! safe — that is exactly what this pass does for referenced-but-unbound names a
//! helpers file exports.

use std::collections::{BTreeMap, BTreeSet};

use crate::EmitPlan;
use crate::complete_referenced_imports::module_exported_names;
use crate::local_bindings::local_bindings_in_source;
use crate::relative_paths::relative_import_specifier;
use crate::runtime_globals::is_runtime_global_identifier;
use crate::runtime_source_scan::value_identifiers_in_source;
use crate::statement_parsers::parse_generated_named_import_specifiers;

/// Returns the number of helper imports added.
pub(crate) fn complete_runtime_helper_imports(plan: &mut EmitPlan) -> usize {
    // helper name → the (deterministically chosen) helpers-file path exporting
    // it. Any helpers file defining the shared helper is equivalent, so the
    // lexicographically smallest path keeps the choice stable.
    let mut defining_helpers_path = BTreeMap::<String, String>::new();
    for file in &plan.files {
        if !is_runtime_helpers_path(&file.path) {
            continue;
        }
        for name in module_exported_names(file) {
            defining_helpers_path
                .entry(name)
                .and_modify(|existing| {
                    if file.path < *existing {
                        *existing = file.path.clone();
                    }
                })
                .or_insert_with(|| file.path.clone());
        }
    }
    if defining_helpers_path.is_empty() {
        return 0;
    }

    let mut added = 0usize;
    for file in &mut plan.files {
        if is_runtime_helpers_path(&file.path) {
            continue;
        }
        let body = file.body.join("\n");
        let local = local_bindings_in_source(&body);
        let imported = imported_local_names(&body);
        let referenced = value_identifiers_in_source(&body);

        let mut imports_by_specifier = BTreeMap::<String, BTreeSet<String>>::new();
        for name in referenced {
            if imported.contains(&name)
                || local.contains(&name)
                || is_runtime_global_identifier(&name)
            {
                continue;
            }
            let Some(helpers_path) = defining_helpers_path.get(&name) else {
                continue;
            };
            if helpers_path == &file.path {
                continue;
            }
            let specifier = relative_import_specifier(&file.path, helpers_path);
            imports_by_specifier
                .entry(specifier)
                .or_default()
                .insert(name);
        }
        if imports_by_specifier.is_empty() {
            continue;
        }

        let mut prefix = String::new();
        for (specifier, names) in imports_by_specifier {
            let clause = names.iter().cloned().collect::<Vec<_>>().join(", ");
            prefix.push_str(&format!("import {{ {clause} }} from '{specifier}';\n"));
            added += names.len();
        }
        file.body = vec![format!("{prefix}{body}")];
    }
    added
}

fn is_runtime_helpers_path(path: &str) -> bool {
    path.contains("/runtime/") && path.ends_with("-helpers.ts")
}

/// Local binding names introduced by the generated `import { … } from '…';`
/// statements in a body (the alias side of `a as b` is the local).
fn imported_local_names(body: &str) -> BTreeSet<String> {
    let mut locals = BTreeSet::new();
    for line in body.lines() {
        if let Some((specifiers, _specifier)) = parse_generated_named_import_specifiers(line.trim())
        {
            for specifier in specifiers {
                locals.insert(specifier.local.as_str().to_string());
            }
        }
    }
    locals
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::PlannedFile;

    #[test]
    fn imports_referenced_helper_from_defining_helpers_file() {
        let mut plan = EmitPlan::default();
        let mut helpers = PlannedFile::new("modules/runtime/source-6-helpers.ts");
        helpers.push_source("function st(e, A) { return () => e; }\nexport { st };");
        plan.push_file(helpers);
        // A module in another source file that uses `st` with no import.
        let mut user = PlannedFile::new("modules/1539-esbuild-SCr.ts");
        user.push_source("var SCr = st(() => {});\nexport { SCr };");
        plan.push_file(user);

        let added = complete_runtime_helper_imports(&mut plan);
        assert_eq!(added, 1);
        assert!(
            plan.files[1]
                .body
                .join("\n")
                .contains("import { st } from './runtime/source-6-helpers.js';"),
            "{}",
            plan.files[1].body.join("\n")
        );
    }

    #[test]
    fn does_not_reimport_an_already_imported_helper() {
        let mut plan = EmitPlan::default();
        let mut helpers = PlannedFile::new("modules/runtime/source-6-helpers.ts");
        helpers.push_source("function st(e) { return e; }\nexport { st };");
        plan.push_file(helpers);
        let mut user = PlannedFile::new("modules/m.ts");
        user.push_source(
            "import { st } from './runtime/source-6-helpers.js';\nvar x = st(() => {});",
        );
        plan.push_file(user);

        assert_eq!(complete_runtime_helper_imports(&mut plan), 0);
    }

    #[test]
    fn ignores_locally_bound_name_matching_a_helper() {
        let mut plan = EmitPlan::default();
        let mut helpers = PlannedFile::new("modules/runtime/source-6-helpers.ts");
        helpers.push_source("function st(e) { return e; }\nexport { st };");
        plan.push_file(helpers);
        let mut user = PlannedFile::new("modules/m.ts");
        user.push_source("function st() { return 1; }\nvar x = st();");
        plan.push_file(user);

        assert_eq!(complete_runtime_helper_imports(&mut plan), 0);
    }
}
