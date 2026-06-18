//! Cross-module dead-export elimination (whole-program export tree-shaking).
//!
//! `plan_reachability` drops whole files unreachable from `cli.ts`;
//! `runtime_orphan_prune` drops dead *private* bindings inside a module. The gap
//! between them is an *exported* binding that no reachable module ever imports.
//! This pass closes that gap: it builds a whole-program view of which exported
//! names are actually imported (by name) somewhere, removes the export marking
//! from the rest, and then re-runs `runtime_orphan_prune` so the now-private,
//! unreferenced bindings (and their exclusive private closures) drop too.
//!
//! Semantics-first conservatism: only planner-emitted *static, named* imports
//! count as "live". A file reached through a namespace import (`import * as ns`)
//! or a star re-export (`export *`), and the `cli.ts` entry itself, are treated
//! as opaque — all of their exports are kept, because we cannot prove which
//! member a namespace/star consumer touches. Dynamic/computed access is never in
//! the candidate set (the graph only follows static named specifiers), mirroring
//! `plan_reachability`'s "ignore unknown strings rather than guess" rule.

use std::collections::{BTreeMap, BTreeSet};

use reverts_ir::BindingName;

use crate::runtime_orphan_prune::prune_orphan_runtime_bindings;
use crate::{EmitPlan, PlannedFile, apply_text_edits, top_level_statement_spans};

const CLI_ENTRYPOINT_PATH: &str = "cli.ts";

pub(crate) fn prune_dead_exports(plan: &mut EmitPlan) {
    // Only meaningful with a known program entry: without `cli.ts` there is no
    // whole-program closure to compute "imported anywhere" against, so every
    // export must be conservatively kept (mirrors `plan_reachability`). This also
    // leaves library-style emit plans — and unit fixtures without an entry —
    // untouched.
    if !plan
        .files
        .iter()
        .any(|file| file.path == CLI_ENTRYPOINT_PATH)
    {
        return;
    }
    let path_set = plan
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect::<BTreeSet<_>>();

    // Iterate to a fixpoint: dropping a file's import of `./Y.a` can make `Y.a`
    // dead in turn. Each round only ever shrinks the live set, so this
    // terminates; the file count bounds the iterations.
    loop {
        let analysis = WholeProgramExports::build(plan, &path_set);
        let mut changed = false;
        for file in &mut plan.files {
            if analysis.opaque.contains(&file.path) || file.path == CLI_ENTRYPOINT_PATH {
                continue;
            }
            let live = analysis.live_names.get(&file.path);
            if prune_file_dead_exports(file, live) {
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

/// Whole-program view of which exported names are live and which target files
/// must keep every export (opaque).
struct WholeProgramExports {
    /// path -> names imported from it by some file via a static *named* import
    /// or named re-export.
    live_names: BTreeMap<String, BTreeSet<String>>,
    /// paths whose every export must be kept (namespace-imported, star-reexported).
    opaque: BTreeSet<String>,
}

impl WholeProgramExports {
    fn build(plan: &EmitPlan, path_set: &BTreeSet<String>) -> Self {
        let mut live_names = BTreeMap::<String, BTreeSet<String>>::new();
        let mut opaque = BTreeSet::<String>::new();
        for file in &plan.files {
            // Structured planner imports are namespace (`import * as ns`) form,
            // so any relative one makes its target opaque.
            for import in &file.imports {
                if let Some(specifier) = import.resolution.specifier()
                    && let Some(target) =
                        resolve_relative_plan_path(&file.path, specifier, path_set)
                {
                    opaque.insert(target);
                }
            }
            for statement in body_statements(&file.body) {
                match module_item(statement.as_str()) {
                    Some(ModuleItem::NamedImport { specifier, names })
                    | Some(ModuleItem::NamedReexport { specifier, names }) => {
                        if let Some(target) =
                            resolve_relative_plan_path(&file.path, specifier.as_str(), path_set)
                        {
                            live_names.entry(target).or_default().extend(names);
                        }
                    }
                    Some(ModuleItem::OpaqueImport { specifier }) => {
                        if let Some(target) =
                            resolve_relative_plan_path(&file.path, specifier.as_str(), path_set)
                        {
                            opaque.insert(target);
                        }
                    }
                    _ => {}
                }
            }
        }
        Self { live_names, opaque }
    }
}

/// Remove dead exports from one file. Returns true if anything changed.
fn prune_file_dead_exports(file: &mut PlannedFile, live: Option<&BTreeSet<String>>) -> bool {
    let is_live = |name: &str| live.is_some_and(|set| set.contains(name));

    // 1. Structured exports: drop the dead ones outright.
    let before = file.exports.len();
    file.exports
        .retain(|export| is_live(export.binding.as_str()));
    let mut changed = file.exports.len() != before;

    // 2. Body `export { ... }` (local) clauses: strip dead names via delimiter-
    //    aware byte-range edits. Re-exports (`export { ... } from '...'`) are left
    //    intact — their names double as the import edge that keeps the target's
    //    binding live, and rewriting them risks desyncing that edge.
    let joined = file.body.join("\n");
    let mut edits = Vec::<(usize, usize, String)>::new();
    let mut newly_unexported = BTreeSet::<BindingName>::new();
    for (start, end) in top_level_statement_spans(joined.as_str()) {
        let statement = joined[start..end].trim();
        let Some(ModuleItem::NamedExport { names }) = module_item(statement) else {
            continue;
        };
        let (kept, dropped): (Vec<String>, Vec<String>) =
            names.into_iter().partition(|name| is_live(name.as_str()));
        if dropped.is_empty() {
            continue;
        }
        for name in dropped {
            newly_unexported.insert(BindingName::new(name));
        }
        let replacement = if kept.is_empty() {
            String::new()
        } else {
            format!("export {{ {} }};", kept.join(", "))
        };
        edits.push((start, end, replacement));
    }

    if !edits.is_empty() {
        let rewritten = apply_text_edits(joined.as_str(), &edits);
        file.body = vec![rewritten];
        changed = true;
    }

    // 3. Demote complete: re-run orphan pruning so the bindings we just stopped
    //    exporting drop if nothing else in the file references them. Roots are
    //    the names still exported (structured + remaining body exports).
    if !newly_unexported.is_empty() {
        let roots = surviving_export_roots(file);
        let joined = file.body.join("\n");
        let pruned = prune_orphan_runtime_bindings(joined.as_str(), &roots);
        if pruned.source != joined {
            file.body = vec![pruned.source];
            changed = true;
        }
    }

    changed
}

/// The set of binding names a file still exports after pruning — used as roots
/// so orphan pruning keeps them.
fn surviving_export_roots(file: &PlannedFile) -> BTreeSet<BindingName> {
    let mut roots = file
        .exports
        .iter()
        .map(|export| export.binding.clone())
        .collect::<BTreeSet<_>>();
    for statement in body_statements(&file.body) {
        if let Some(ModuleItem::NamedExport { names })
        | Some(ModuleItem::NamedReexport { names, .. }) = module_item(statement.as_str())
        {
            roots.extend(names.into_iter().map(BindingName::new));
        }
    }
    roots
}

enum ModuleItem {
    NamedImport {
        specifier: String,
        names: Vec<String>,
    },
    NamedReexport {
        specifier: String,
        names: Vec<String>,
    },
    NamedExport {
        names: Vec<String>,
    },
    OpaqueImport {
        specifier: String,
    },
}

/// Classify a single trimmed statement into the module-graph item we care about.
fn module_item(statement: &str) -> Option<ModuleItem> {
    // `import * as ns from '...'`  /  `export * from '...'` -> opaque target.
    if (statement.starts_with("import ") && statement.contains("* as "))
        || statement.starts_with("export *")
    {
        return specifier_suffix(statement).map(|specifier| ModuleItem::OpaqueImport { specifier });
    }
    // `export { a, b } from '...'`  (named re-export)
    if statement.starts_with("export {")
        && let Some(specifier) = specifier_suffix(statement)
    {
        return brace_names(statement, true)
            .map(|names| ModuleItem::NamedReexport { specifier, names });
    }
    // `export { a, b };` (local export)
    if statement.starts_with("export {") {
        return brace_names(statement, false).map(|names| ModuleItem::NamedExport { names });
    }
    // `import { a, b } from '...'`  /  `import def, { a } from '...'`
    if statement.starts_with("import ")
        && statement.contains('{')
        && let Some(specifier) = specifier_suffix(statement)
    {
        return brace_names(statement, true)
            .map(|names| ModuleItem::NamedImport { specifier, names });
    }
    None
}

/// Extract the `'...'` module specifier following ` from ` (or trailing the
/// statement for `import 'x'`/`export * from 'x'`).
fn specifier_suffix(statement: &str) -> Option<String> {
    let (_head, rest) = statement.rsplit_once(" from ")?;
    let rest = rest.trim();
    let inner = rest
        .strip_prefix('\'')
        .and_then(|r| r.split('\'').next())
        .or_else(|| rest.strip_prefix('"').and_then(|r| r.split('"').next()))?;
    Some(inner.to_string())
}

/// Names inside the first `{ ... }` of an import/export clause. For import
/// clauses the *imported* (left of `as`) name is what the target exports; for
/// export clauses the *exported* (right of `as`, or bare) name is what the file
/// exposes. `import_side` selects which.
fn brace_names(statement: &str, import_side: bool) -> Option<Vec<String>> {
    let open = statement.find('{')?;
    let close = statement[open..].find('}')? + open;
    let names = statement[open + 1..close]
        .split(',')
        .filter_map(|part| {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }
            let name = match part.split_once(" as ") {
                Some((left, right)) => {
                    if import_side {
                        left.trim()
                    } else {
                        right.trim()
                    }
                }
                None => part,
            };
            (is_identifier(name)).then(|| name.to_string())
        })
        .collect::<Vec<_>>();
    Some(names)
}

fn is_identifier(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c == '_' || c == '$' || c.is_ascii_alphabetic())
        && name
            .chars()
            .all(|c| c == '_' || c == '$' || c.is_ascii_alphanumeric())
}

/// Parser-derived top-level statements of a file body. Delimiter-aware (unlike a
/// naive `;` split, which shreds function/object bodies) so that import/export
/// classification and clause rewriting see whole statements.
fn body_statements(body: &[String]) -> Vec<String> {
    let joined = body.join("\n");
    top_level_statement_spans(joined.as_str())
        .into_iter()
        .map(|(start, end)| joined[start..end].trim().to_string())
        .filter(|statement| !statement.is_empty())
        .collect()
}

fn resolve_relative_plan_path(
    file_path: &str,
    specifier: &str,
    path_set: &BTreeSet<String>,
) -> Option<String> {
    if !(specifier.starts_with("./") || specifier.starts_with("../")) {
        return None;
    }
    let directory = file_path
        .rsplit_once('/')
        .map_or("", |(directory, _)| directory);
    let mut parts = if directory.is_empty() {
        Vec::<&str>::new()
    } else {
        directory.split('/').collect::<Vec<_>>()
    };
    for part in specifier.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    let mut path = parts.join("/");
    if let Some(stripped) = path.strip_suffix(".js") {
        path = format!("{stripped}.ts");
    }
    path_set.contains(&path).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, body: &str) -> PlannedFile {
        let mut file = PlannedFile::new(path);
        file.push_source(body);
        file
    }

    fn body_of<'a>(plan: &'a EmitPlan, path: &str) -> &'a str {
        plan.files
            .iter()
            .find(|file| file.path == path)
            .map(|file| file.body[0].as_str())
            .expect("file present")
    }

    #[test]
    fn drops_dead_local_export_and_prunes_its_binding() {
        let mut plan = EmitPlan::default();
        plan.push_file(file("cli.ts", "import { keep } from './modules/a.js';"));
        plan.push_file(file(
            "modules/a.ts",
            "var keep = 1;\nvar dead = 2;\nexport { keep, dead };",
        ));

        prune_dead_exports(&mut plan);

        let a = body_of(&plan, "modules/a.ts");
        assert!(a.contains("export { keep }"), "{a}");
        assert!(
            !a.contains("dead"),
            "dead binding + export must be gone: {a}"
        );
        assert!(a.contains("var keep = 1"), "{a}");
    }

    #[test]
    fn keeps_all_exports_of_namespace_imported_file() {
        let mut plan = EmitPlan::default();
        plan.push_file(file("cli.ts", "import * as ns from './modules/a.js';"));
        plan.push_file(file(
            "modules/a.ts",
            "var x = 1;\nvar y = 2;\nexport { x, y };",
        ));

        prune_dead_exports(&mut plan);

        let a = body_of(&plan, "modules/a.ts");
        assert!(
            a.contains("export { x, y }"),
            "namespace import keeps all: {a}"
        );
    }

    #[test]
    fn keeps_all_exports_of_star_reexported_file() {
        let mut plan = EmitPlan::default();
        plan.push_file(file("cli.ts", "export * from './modules/a.js';"));
        plan.push_file(file(
            "modules/a.ts",
            "var x = 1;\nvar y = 2;\nexport { x, y };",
        ));

        prune_dead_exports(&mut plan);

        assert!(body_of(&plan, "modules/a.ts").contains("export { x, y }"));
    }

    #[test]
    fn never_prunes_the_entry_file_exports() {
        let mut plan = EmitPlan::default();
        plan.push_file(file("cli.ts", "var a = 1;\nexport { a };"));

        prune_dead_exports(&mut plan);

        assert!(body_of(&plan, "cli.ts").contains("export { a }"));
    }

    #[test]
    fn named_reexport_keeps_target_binding_live() {
        let mut plan = EmitPlan::default();
        plan.push_file(file("cli.ts", "import { wrapped } from './modules/b.js';"));
        // b re-exports a's `inner` as `wrapped`; a must keep `inner`.
        plan.push_file(file(
            "modules/b.ts",
            "export { inner as wrapped } from './a.js';",
        ));
        plan.push_file(file("modules/a.ts", "var inner = 1;\nexport { inner };"));

        prune_dead_exports(&mut plan);

        assert!(
            body_of(&plan, "modules/a.ts").contains("export { inner }"),
            "re-exported binding must stay live"
        );
    }

    #[test]
    fn dropping_dead_export_cascades_to_its_private_closure() {
        // `dead` is exported but never imported; it is the sole caller of the
        // private `deadHelper`. Dropping the export demotes `dead`, and orphan
        // pruning then removes both `dead` and its now-unreferenced helper
        // (transitive reachability over prunable function declarations).
        let mut plan = EmitPlan::default();
        plan.push_file(file("cli.ts", "import { keep } from './modules/a.js';"));
        plan.push_file(file(
            "modules/a.ts",
            "function keep() { return 1; }\nfunction deadHelper() { return 5; }\nfunction dead() { return deadHelper(); }\nexport { keep, dead };",
        ));

        prune_dead_exports(&mut plan);

        let a = body_of(&plan, "modules/a.ts");
        assert!(a.contains("function keep()"), "{a}");
        assert!(a.contains("export { keep }"), "{a}");
        assert!(!a.contains("function dead()"), "dead export pruned: {a}");
        assert!(!a.contains("deadHelper"), "its private closure pruned: {a}");
    }

    #[test]
    fn preserves_import_statements_to_keep_module_load_side_effects() {
        // Even when a named import becomes unreferenced after pruning, the import
        // is kept: dropping it could remove an observable module-load side effect.
        let mut plan = EmitPlan::default();
        plan.push_file(file("cli.ts", "import { keep } from './modules/a.js';"));
        plan.push_file(file(
            "modules/a.ts",
            "import { mid } from './b.js';\nvar keep = 1;\nvar dead = 2;\nexport { keep, dead };",
        ));
        plan.push_file(file("modules/b.ts", "var mid = 9;\nexport { mid };"));

        prune_dead_exports(&mut plan);

        assert!(
            body_of(&plan, "modules/a.ts").contains("import { mid } from './b.js'"),
            "import preserved for side-effect safety"
        );
    }

    #[test]
    fn drops_structured_dead_export() {
        let mut plan = EmitPlan::default();
        plan.push_file(file("cli.ts", "import { live } from './modules/a.js';"));
        let mut a = file("modules/a.ts", "var live = 1;");
        a.add_export(BindingName::new("live"));
        a.add_export(BindingName::new("deadStructured"));
        plan.push_file(a);

        prune_dead_exports(&mut plan);

        let a = plan
            .files
            .iter()
            .find(|file| file.path == "modules/a.ts")
            .expect("a present");
        let exported = a
            .exports
            .iter()
            .map(|export| export.binding.as_str())
            .collect::<Vec<_>>();
        assert_eq!(exported, vec!["live"], "dead structured export removed");
    }
}
