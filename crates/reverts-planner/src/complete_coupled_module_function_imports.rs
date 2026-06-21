//! Completion pass: wire a referenced-but-unbound name to the module that
//! top-level-defines it as a FUNCTION, when the two modules are ALREADY coupled
//! by an existing import edge (so they were the same original scope/bundle).
//!
//! esbuild can split one source module across the planner's prelude/module
//! boundary: e.g. execa's main `aut` lands in `runtime/source-6-helpers.ts`
//! while the helpers it calls (`sut`, `iut`, `rut`, `PNe`, …) land in sliced
//! module `914-esbuild-jdr.ts`, which ALREADY imports back from
//! `source-6-helpers.ts`. The result is `aut` referencing `sut` with no import
//! → `ReferenceError` at runtime. Neither [`complete_referenced_module_imports`]
//! (no existing consumer→definer edge to augment) nor
//! [`complete_runtime_helper_imports`] (definer is not a helpers file and does
//! not even export the name) repairs it.
//!
//! This pass adds a NEW import edge, which is normally avoided for cycle-safety.
//! It is sound here because it fires ONLY when:
//!   * the name is bound nowhere in the consumer, and is not a runtime/JS global;
//!   * exactly ONE module top-level-defines it as a FUNCTION (callable, so it is
//!     used via call sites → deferred → live even if the new edge closes a cycle);
//!   * that module is ALREADY coupled to the consumer by an existing import edge
//!     in either direction (proving same scope/bundle — never wires across
//!     independent bundles).
//!
//! The missing `export` on the definer is added afterwards by
//! [`crate::export_completion::complete_cross_module_exports`].
//!
//! [`complete_referenced_module_imports`]: crate::complete_referenced_imports::complete_referenced_module_imports
//! [`complete_runtime_helper_imports`]: crate::complete_runtime_helper_imports::complete_runtime_helper_imports

use std::collections::{BTreeMap, BTreeSet};

use crate::EmitPlan;
use crate::complete_referenced_imports::resolve_relative_specifier;
use crate::local_bindings::local_bindings_in_source;
use crate::relative_paths::relative_import_specifier;
use crate::runtime_globals::is_runtime_global_identifier;
use crate::runtime_source_scan::{top_level_function_valued_names, value_identifiers_in_source};
use crate::statement_parsers::parse_generated_named_import_specifiers;

/// Returns the number of coupled-module function imports added.
pub(crate) fn complete_coupled_module_function_imports(plan: &mut EmitPlan) -> usize {
    // name → the module paths that top-level-define it as a function.
    let mut function_definers = BTreeMap::<String, BTreeSet<String>>::new();
    // module path → the set of module paths it imports from (edge adjacency).
    let mut imports_from = BTreeMap::<String, BTreeSet<String>>::new();
    for file in &plan.files {
        let body = file.body.join("\n");
        for name in top_level_function_valued_names(&body) {
            function_definers
                .entry(name)
                .or_default()
                .insert(file.path.clone());
        }
        let mut targets = BTreeSet::new();
        for line in body.lines() {
            if let Some((_, specifier)) = parse_generated_named_import_specifiers(line.trim())
                && let Some(target) = resolve_relative_specifier(&file.path, &specifier)
            {
                targets.insert(target);
            }
        }
        imports_from.insert(file.path.clone(), targets);
    }

    // Only names with a SINGLE function definer are wireable (a minified name
    // defined in two modules is ambiguous — never guess).
    let unique_definer: BTreeMap<String, String> = function_definers
        .into_iter()
        .filter(|(_, paths)| paths.len() == 1)
        .map(|(name, paths)| (name, paths.into_iter().next().expect("len == 1")))
        .collect();
    if unique_definer.is_empty() {
        return 0;
    }

    let mut added = 0usize;
    for file in &mut plan.files {
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
            let Some(definer) = unique_definer.get(&name) else {
                continue;
            };
            if definer == &file.path {
                continue;
            }
            // Require an existing import edge in either direction: it proves the
            // two modules are the same original scope/bundle, and bounds the new
            // edge to an already-coupled pair (the cycle it may close is between
            // modules that already depend on each other).
            let coupled = imports_from
                .get(&file.path)
                .is_some_and(|targets| targets.contains(definer))
                || imports_from
                    .get(definer)
                    .is_some_and(|targets| targets.contains(&file.path));
            if !coupled {
                continue;
            }
            let specifier = relative_import_specifier(&file.path, definer);
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
    fn wires_unbound_function_to_coupled_definer_via_reverse_edge() {
        // Mirrors execa: `aut` (consumer) calls `sut` with no import; `sut` is a
        // top-level arrow defined in the definer, which ALREADY imports back from
        // the consumer (the reverse edge). The pass adds the forward import.
        let mut plan = EmitPlan::default();
        let mut consumer = PlannedFile::new("modules/runtime/source-6-helpers.ts");
        consumer.push_source("function aut(e) { return sut(e); }\nexport { aut };");
        plan.push_file(consumer);
        let mut definer = PlannedFile::new("modules/914-esbuild-jdr.ts");
        definer.push_source(
            "import { aut } from './runtime/source-6-helpers.js';\nvar sut = (e) => e + 1;",
        );
        plan.push_file(definer);

        let added = complete_coupled_module_function_imports(&mut plan);
        assert_eq!(added, 1);
        assert!(
            plan.files[0]
                .body
                .join("\n")
                .contains("import { sut } from '../914-esbuild-jdr.js';"),
            "{}",
            plan.files[0].body.join("\n")
        );
    }

    #[test]
    fn does_not_wire_when_modules_are_not_coupled() {
        // No existing edge in either direction → independent bundles → never add
        // a cross-bundle edge.
        let mut plan = EmitPlan::default();
        let mut consumer = PlannedFile::new("modules/a.ts");
        consumer.push_source("function run() { return sut(1); }");
        plan.push_file(consumer);
        let mut definer = PlannedFile::new("modules/b.ts");
        definer.push_source("var sut = (e) => e;");
        plan.push_file(definer);

        assert_eq!(complete_coupled_module_function_imports(&mut plan), 0);
    }

    #[test]
    fn does_not_wire_ambiguous_name_defined_in_two_modules() {
        let mut plan = EmitPlan::default();
        let mut consumer = PlannedFile::new("modules/a.ts");
        consumer.push_source("import { x } from './b.js';\nfunction run() { return sut(1); }");
        plan.push_file(consumer);
        let mut b = PlannedFile::new("modules/b.ts");
        b.push_source("var x = 1;\nvar sut = (e) => e;");
        plan.push_file(b);
        let mut c = PlannedFile::new("modules/c.ts");
        c.push_source("import { x } from './b.js';\nvar sut = (e) => e * 2;");
        plan.push_file(c);

        assert_eq!(complete_coupled_module_function_imports(&mut plan), 0);
    }

    #[test]
    fn does_not_wire_non_function_value() {
        // A data-valued binding is not eligible (eager use across the new edge
        // could be undefined at load); only function values are deferred-safe.
        let mut plan = EmitPlan::default();
        let mut consumer = PlannedFile::new("modules/a.ts");
        consumer.push_source("function run() { return DATA; }");
        plan.push_file(consumer);
        let mut definer = PlannedFile::new("modules/b.ts");
        definer.push_source("import { run } from './a.js';\nvar DATA = { x: 1 };");
        plan.push_file(definer);

        assert_eq!(complete_coupled_module_function_imports(&mut plan), 0);
    }
}
