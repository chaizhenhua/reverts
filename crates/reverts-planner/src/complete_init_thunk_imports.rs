//! Completion pass: wire a referenced-but-unbound esbuild init-thunk call to its
//! defining module by adding a NEW import.
//!
//! The eager entrypoint of an esbuild bundle initializes lazy modules by calling
//! their `__esm` init thunks (`cdA()`, `SI()`, …). When normal wiring records the
//! edge for some thunks but misses one — e.g. `cdA` owned by a sliced module the
//! entrypoint has no OTHER edge to — the call is left unbound → `ReferenceError:
//! cdA is not defined` at load. Neither [`complete_referenced_module_imports`]
//! (no existing edge to augment) nor [`complete_coupled_module_function_imports`]
//! (the thunk is `var X = nt(…)`, not a function declaration, and the pair is not
//! edge-coupled) repairs it.
//!
//! Adding a NEW edge is sound here because the binding is an init thunk:
//!   * it is used only via a DEFERRED call, so it stays live even if the new edge
//!     closes an ESM init cycle;
//!   * it is defined as a thunk in exactly ONE module (unambiguous);
//!   * consumer and definer both import the SAME bundle runtime-helpers file,
//!     proving one original esbuild bundle — so the edge never crosses into an
//!     independent bundle.
//!
//! Imports are read from parser-derived statement spans, not `body.lines()`: the
//! lowered entrypoint island concatenates every import onto a single line.
//!
//! [`complete_referenced_module_imports`]: crate::complete_referenced_imports::complete_referenced_module_imports
//! [`complete_coupled_module_function_imports`]: crate::complete_coupled_module_function_imports::complete_coupled_module_function_imports

use std::collections::{BTreeMap, BTreeSet};

use crate::complete_referenced_imports::resolve_relative_specifier;
use crate::local_bindings::local_bindings_in_source;
use crate::relative_paths::relative_import_specifier;
use crate::runtime_globals::is_runtime_global_identifier;
use crate::runtime_source_scan::{top_level_init_thunk_names, value_identifiers_in_source};
use crate::statement_parsers::parse_generated_named_import_specifiers;
use crate::{EmitPlan, top_level_statement_spans};

pub(crate) fn complete_init_thunk_imports(plan: &mut EmitPlan) -> usize {
    let mut thunk_definers = BTreeMap::<String, BTreeSet<String>>::new();
    let mut runtime_helper_imports = BTreeMap::<String, BTreeSet<String>>::new();
    for file in &plan.files {
        let body = file.body.join("\n");
        for name in top_level_init_thunk_names(body.as_str()) {
            thunk_definers
                .entry(name)
                .or_default()
                .insert(file.path.clone());
        }
        let helpers = imported_specifiers(body.as_str())
            .filter_map(|specifier| resolve_relative_specifier(&file.path, &specifier))
            .filter(|target| is_runtime_helpers_path(target))
            .collect::<BTreeSet<_>>();
        runtime_helper_imports.insert(file.path.clone(), helpers);
    }
    let unique_definer: BTreeMap<String, String> = thunk_definers
        .into_iter()
        .filter(|(_, paths)| paths.len() == 1)
        .map(|(name, paths)| (name, paths.into_iter().next().expect("len == 1")))
        .collect();
    if unique_definer.is_empty() {
        return 0;
    }

    // The eager entrypoint island and the cluster/chunk files split out of it are
    // ONE esbuild bundle, but only the island file keeps the bundle's shared
    // runtime-helpers import — the split moves an init-thunk CALL (`cdA()`) into a
    // cluster without that helper import, so the per-file `same_bundle` gate below
    // would wrongly reject wiring the call to its definer. Union the helper imports
    // of every island-derived file (`unmodularized_recovered_code`) so each such
    // file is treated as carrying the whole island bundle's helpers.
    let island_helpers: BTreeSet<String> = plan
        .files
        .iter()
        .filter(|file| file.unmodularized_recovered_code)
        .filter_map(|file| runtime_helper_imports.get(&file.path))
        .flatten()
        .cloned()
        .collect();

    let mut added = 0usize;
    for file in &mut plan.files {
        let body = file.body.join("\n");
        let local = local_bindings_in_source(body.as_str());
        let imported = imported_local_names(body.as_str());
        let mut consumer_helpers = runtime_helper_imports
            .get(&file.path)
            .cloned()
            .unwrap_or_default();
        if file.unmodularized_recovered_code {
            consumer_helpers.extend(island_helpers.iter().cloned());
        }

        let mut imports_by_specifier = BTreeMap::<String, BTreeSet<String>>::new();
        for name in value_identifiers_in_source(body.as_str()) {
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
            let same_bundle = runtime_helper_imports
                .get(definer)
                .is_some_and(|definer_helpers| {
                    definer_helpers
                        .iter()
                        .any(|helper| consumer_helpers.contains(helper))
                });
            if !same_bundle {
                continue;
            }
            imports_by_specifier
                .entry(relative_import_specifier(&file.path, definer))
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
    path.contains("runtime/") && path.contains("helpers")
}

/// The `from '…'` specifier of every named import in a body, using parser-derived
/// statement spans (the island body concatenates imports onto one line).
fn imported_specifiers(body: &str) -> impl Iterator<Item = String> + '_ {
    top_level_statement_spans(body)
        .into_iter()
        .filter_map(move |(start, end)| {
            parse_generated_named_import_specifiers(body[start..end].trim())
                .map(|(_, specifier)| specifier)
        })
        .collect::<Vec<_>>()
        .into_iter()
}

/// Local binding names introduced by named imports (the `local` side of `a as b`).
fn imported_local_names(body: &str) -> BTreeSet<String> {
    top_level_statement_spans(body)
        .into_iter()
        .filter_map(|(start, end)| {
            parse_generated_named_import_specifiers(body[start..end].trim()).map(|(specs, _)| specs)
        })
        .flatten()
        .map(|spec| spec.local.as_str().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::PlannedFile;

    fn body_of(plan: &EmitPlan, path: &str) -> String {
        plan.files
            .iter()
            .find(|file| file.path == path)
            .map(|file| file.body.join("\n"))
            .expect("file present")
    }

    #[test]
    fn wires_unbound_init_thunk_call_to_its_definer() {
        // The entrypoint calls `cdA()` (an `__esm` init thunk) with no import.
        // `cdA` is uniquely defined in module 461; both files share the bundle
        // runtime-helpers import, so the pass adds the missing import.
        let mut plan = EmitPlan::default();
        let mut entry = PlannedFile::new("modules/entrypoint.ts");
        entry.push_source(
            "import { nt } from './runtime/source-6-helpers.js';import { SI } from './460.js';\ncdA();\nSI();",
        );
        plan.push_file(entry);
        let mut def = PlannedFile::new("modules/461-esbuild-cdA.ts");
        def.push_source(
            "import { nt } from './runtime/source-6-helpers.js';\nvar PDt, cdA = nt(() => { PDt = 1; });\nexport { cdA };",
        );
        plan.push_file(def);

        let added = complete_init_thunk_imports(&mut plan);
        assert_eq!(added, 1, "{}", body_of(&plan, "modules/entrypoint.ts"));
        assert!(
            body_of(&plan, "modules/entrypoint.ts")
                .contains("import { cdA } from './461-esbuild-cdA.js';"),
            "{}",
            body_of(&plan, "modules/entrypoint.ts")
        );
    }

    #[test]
    fn does_not_wire_across_independent_bundles() {
        // No shared runtime-helpers import → different bundles → never wire.
        let mut plan = EmitPlan::default();
        let mut entry = PlannedFile::new("modules/a.ts");
        entry.push_source("import { x } from './x.js';\ncdA();");
        plan.push_file(entry);
        let mut def = PlannedFile::new("modules/b.ts");
        def.push_source(
            "import { nt } from './runtime/other-helpers.js';\nvar cdA = nt(() => {});\nexport { cdA };",
        );
        plan.push_file(def);

        assert_eq!(complete_init_thunk_imports(&mut plan), 0);
    }

    #[test]
    fn wires_init_thunk_call_that_split_into_a_cluster_without_the_helper_import() {
        // The eager entrypoint island and a cluster split out of it are ONE
        // bundle, but only the island file kept the shared runtime-helpers import.
        // A `cdA()` call that moved into the cluster must still be wired to its
        // definer (module 461), even though the cluster imports no runtime helper.
        let mut plan = EmitPlan::default();
        let mut entry = PlannedFile::new("modules/entrypoint.ts");
        entry.push_source("import { nt } from './runtime/source-6-helpers.js';\nnt();");
        entry.unmodularized_recovered_code = true;
        plan.push_file(entry);
        let mut cluster = PlannedFile::new("modules/island/cluster-2964.ts");
        // No runtime-helpers import of its own — the split left it behind.
        cluster.push_source("function run() { return cdA(); }\nexport { run };");
        cluster.unmodularized_recovered_code = true;
        plan.push_file(cluster);
        let mut def = PlannedFile::new("modules/461-esbuild-cdA.ts");
        def.push_source(
            "import { nt } from './runtime/source-6-helpers.js';\nvar PDt, cdA = nt(() => { PDt = 1; });\nexport { cdA };",
        );
        plan.push_file(def);

        let added = complete_init_thunk_imports(&mut plan);
        assert_eq!(
            added,
            1,
            "{}",
            body_of(&plan, "modules/island/cluster-2964.ts")
        );
        assert!(
            body_of(&plan, "modules/island/cluster-2964.ts")
                .contains("import { cdA } from '../461-esbuild-cdA.js';"),
            "{}",
            body_of(&plan, "modules/island/cluster-2964.ts")
        );
    }

    #[test]
    fn does_not_wire_a_plain_value_binding() {
        // `cdA` here is data, not an init thunk — eager use across a new edge
        // could be undefined at load, so it is not eligible.
        let mut plan = EmitPlan::default();
        let mut entry = PlannedFile::new("modules/entrypoint.ts");
        entry.push_source("import { nt } from './runtime/source-6-helpers.js';\nlet z = cdA;");
        plan.push_file(entry);
        let mut def = PlannedFile::new("modules/b.ts");
        def.push_source(
            "import { nt } from './runtime/source-6-helpers.js';\nvar cdA = { x: 1 };\nexport { cdA };",
        );
        plan.push_file(def);

        assert_eq!(complete_init_thunk_imports(&mut plan), 0);
    }
}
