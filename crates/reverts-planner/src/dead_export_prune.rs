//! Export hygiene: cross-module dead-export elimination + invalid-export pruning.
//!
//! Two passes live here. `prune_dead_exports` (whole-program tree-shaking) and
//! `prune_invalid_exports` (a correctness invariant): a local `export { X }`
//! whose `X` is neither defined nor imported in the module is invalid ESM that
//! crashes at load (`SyntaxError: Export 'X' is not defined`). Adapter emission
//! can leave such dangling exports (e.g. an externalized package adapter that
//! binds only some of a module's original export names); this pass removes them
//! so the emitted program loads.
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

use reverts_ir::{BindingName, is_identifier_like_ascii};

use crate::runtime_orphan_prune::prune_orphan_runtime_bindings;
use crate::{
    EmitPlan, PlannedFile, apply_text_edits, top_level_definitions_in_source,
    top_level_statement_spans,
};

const CLI_ENTRYPOINT_PATH: &str = "cli.ts";

/// Drop local `export { … }` names (and structured exports) that are neither
/// defined nor imported in their module. Such an export is invalid ESM and
/// crashes the program at module load, so this runs unconditionally (it is a
/// correctness fix, not a whole-program optimization like `prune_dead_exports`).
pub(crate) fn prune_invalid_exports(plan: &mut EmitPlan) {
    for file in &mut plan.files {
        let backed = module_backed_names(file);
        let is_backed = |name: &str| backed.contains(name);

        let before = file.exports.len();
        file.exports
            .retain(|export| is_backed(export.binding.as_str()));
        let mut changed = file.exports.len() != before;

        let joined = file.body.join("\n");
        let mut edits = Vec::<(usize, usize, String)>::new();
        for (start, end) in top_level_statement_spans(joined.as_str()) {
            let statement = joined[start..end].trim();
            let Some(ModuleItem::NamedExport { specifiers }) = module_item(statement) else {
                continue;
            };
            let kept = specifiers
                .iter()
                .filter(|specifier| is_backed(specifier.local.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            if kept.len() == specifiers.len() {
                continue;
            }
            let replacement = if kept.is_empty() {
                String::new()
            } else {
                format!("export {{ {} }};", render_export_specifiers(&kept))
            };
            edits.push((start, end, replacement));
        }
        if !edits.is_empty() {
            file.body = vec![apply_text_edits(joined.as_str(), &edits)];
            changed = true;
        }
        let _ = changed;
    }
}

/// Names a module legitimately backs: every top-level definition in its body,
/// every local binding introduced by an import (named/namespace/default), the
/// structured planner namespace imports the emitter injects, and every
/// readability-rename *target* (a defined binding is emitted under its renamed
/// name, so e.g. `export { a as createClient }` is backed by `a` -> `createClient`).
fn module_backed_names(file: &PlannedFile) -> BTreeSet<String> {
    let mut backed = BTreeSet::<String>::new();
    for import in &file.imports {
        backed.insert(import.namespace.as_str().to_string());
    }
    for rename in &file.readability_renames {
        backed.insert(rename.renamed.as_str().to_string());
    }
    let joined = file.body.join("\n");
    for (start, end) in top_level_statement_spans(joined.as_str()) {
        let statement = joined[start..end].trim();
        for definition in top_level_definitions_in_source(statement) {
            backed.insert(definition.as_str().to_string());
        }
        for local in import_local_names(statement) {
            backed.insert(local);
        }
    }
    backed
}

/// Local binding names introduced by an import statement: `* as ns`, the
/// right-hand side of `{ a as b }` (or bare `{ a }`), and a default import.
fn import_local_names(statement: &str) -> Vec<String> {
    if !statement.starts_with("import ") {
        return Vec::new();
    }
    let mut names = Vec::new();
    if let Some((_, after)) = statement.split_once("* as ") {
        let ns = after
            .trim_start()
            .chars()
            .take_while(|character| {
                *character == '_' || *character == '$' || character.is_ascii_alphanumeric()
            })
            .collect::<String>();
        if is_identifier_like_ascii(ns.as_str()) {
            names.push(ns);
        }
    }
    if let Some(open) = statement.find('{')
        && let Some(close) = statement[open..].find('}').map(|index| open + index)
    {
        for part in statement[open + 1..close].split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let local = part
                .split_once(" as ")
                .map(|(_, right)| right.trim())
                .unwrap_or(part);
            if is_identifier_like_ascii(local) {
                names.push(local.to_string());
            }
        }
    }
    if let Some(rest) = statement.strip_prefix("import ") {
        let head = rest.trim_start();
        if !head.starts_with(['{', '*', '\'', '"']) {
            let default = head
                .chars()
                .take_while(|character| {
                    *character == '_' || *character == '$' || character.is_ascii_alphanumeric()
                })
                .collect::<String>();
            if is_identifier_like_ascii(default.as_str()) {
                names.push(default);
            }
        }
    }
    names
}

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
        let Some(ModuleItem::NamedExport { specifiers }) = module_item(statement) else {
            continue;
        };
        let (kept, dropped): (Vec<LocalExportSpecifier>, Vec<LocalExportSpecifier>) = specifiers
            .into_iter()
            .partition(|specifier| is_live(specifier.exported.as_str()));
        if dropped.is_empty() {
            continue;
        }
        for specifier in dropped {
            newly_unexported.insert(BindingName::new(specifier.local));
        }
        let replacement = if kept.is_empty() {
            String::new()
        } else {
            format!("export {{ {} }};", render_export_specifiers(&kept))
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
        if let Some(ModuleItem::NamedExport { specifiers }) = module_item(statement.as_str()) {
            roots.extend(
                specifiers
                    .into_iter()
                    .map(|specifier| BindingName::new(specifier.local)),
            );
        }
    }
    roots
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LocalExportSpecifier {
    local: String,
    exported: String,
}

impl LocalExportSpecifier {
    fn render(&self) -> String {
        if self.local == self.exported {
            self.local.clone()
        } else {
            format!("{} as {}", self.local, self.exported)
        }
    }
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
        specifiers: Vec<LocalExportSpecifier>,
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
        return local_export_specifiers(statement)
            .map(|specifiers| ModuleItem::NamedExport { specifiers });
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
    Some(
        local_export_specifiers(statement)?
            .into_iter()
            .map(|specifier| {
                if import_side {
                    specifier.local
                } else {
                    specifier.exported
                }
            })
            .collect(),
    )
}

fn local_export_specifiers(statement: &str) -> Option<Vec<LocalExportSpecifier>> {
    let open = statement.find('{')?;
    let close = statement[open..].find('}')? + open;
    let specifiers = statement[open + 1..close]
        .split(',')
        .filter_map(|part| {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }
            let (local, exported) = match part.split_once(" as ") {
                Some((left, right)) => (left.trim(), right.trim()),
                None => (part, part),
            };
            (is_identifier_like_ascii(local) && is_identifier_like_ascii(exported)).then(|| {
                LocalExportSpecifier {
                    local: local.to_string(),
                    exported: exported.to_string(),
                }
            })
        })
        .collect::<Vec<_>>();
    Some(specifiers)
}

fn render_export_specifiers(specifiers: &[LocalExportSpecifier]) -> String {
    specifiers
        .iter()
        .map(LocalExportSpecifier::render)
        .collect::<Vec<_>>()
        .join(", ")
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
    fn prunes_export_of_undefined_binding() {
        // Mirrors the externalized-zod adapter bug: the module defines `keep`
        // and namespace-imports `external_zod`, but its export clause also lists
        // `Zk`, which is never defined or imported. Exporting it is invalid ESM
        // (`SyntaxError: Export 'Zk' is not defined`); the pass must drop it.
        let mut plan = EmitPlan::default();
        plan.push_file(file(
            "modules/a.ts",
            "import * as external_zod from 'zod';\nconst keep = external_zod;\nexport { keep, Zk };",
        ));

        prune_invalid_exports(&mut plan);

        let a = body_of(&plan, "modules/a.ts");
        assert!(a.contains("export { keep }"), "{a}");
        assert!(!a.contains("Zk"), "undefined export must be dropped: {a}");
    }

    #[test]
    fn keeps_exports_backed_by_definition_or_import() {
        // `keep` is defined; `aliased` is an import-local binding; both valid.
        let mut plan = EmitPlan::default();
        plan.push_file(file(
            "modules/a.ts",
            "import { orig as aliased } from './b.js';\nfunction keep() {}\nexport { keep, aliased };",
        ));

        prune_invalid_exports(&mut plan);

        let a = body_of(&plan, "modules/a.ts");
        assert!(
            a.contains("keep") && a.contains("aliased"),
            "valid exports kept: {a}"
        );
    }

    #[test]
    fn keeps_local_export_alias_when_local_binding_is_defined() {
        let mut plan = EmitPlan::default();
        plan.push_file(file(
            "modules/a.ts",
            "const a = 1;\nexport { a as createClient };",
        ));

        prune_invalid_exports(&mut plan);

        let a = body_of(&plan, "modules/a.ts");
        assert!(
            a.contains("export { a as createClient }"),
            "local alias validity is decided by the local binding: {a}"
        );
    }

    #[test]
    fn dead_export_prune_preserves_live_local_alias_clause() {
        let mut plan = EmitPlan::default();
        plan.push_file(file(
            "cli.ts",
            "import { createClient } from './modules/a.js';",
        ));
        plan.push_file(file(
            "modules/a.ts",
            "const a = 1;\nconst dead = 2;\nexport { a as createClient, dead as unused };",
        ));

        prune_dead_exports(&mut plan);

        let a = body_of(&plan, "modules/a.ts");
        assert!(a.contains("export { a as createClient }"), "{a}");
        assert!(!a.contains("unused"), "dead alias export is dropped: {a}");
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
