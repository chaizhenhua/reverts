//! Flag export/import *wire* renames that are provably safe to apply
//! project-wide.
//!
//! A local readability rename leaves the public wire name aliased
//! (`export { semantic as Cb }`, `import { Cb as semantic }`). Renaming the wire
//! name itself is only safe when every consumer of that export is updated in
//! lockstep — which the emitter's wire pass does for direct named imports, but
//! NOT for re-export barrels, namespace imports, or dynamic access. This pass
//! decides, with a deliberately conservative global gate, which
//! `(original_wire_name, semantic_name)` pairs are safe, then flags the matching
//! readability renames so the emitter collapses their alias.
//!
//! The export side (in the defining module) and every import side (in
//! consumers) carry the SAME `(original, semantic)` pair, so flagging by pair
//! keeps both ends consistent without any cross-file threading.

use std::collections::{BTreeMap, BTreeSet};

use reverts_ir::{BindingName, ModuleId, ModuleKind};
use reverts_model::EnrichedProgram;

use crate::EmitPlan;

/// Flag readability renames whose `(original, renamed)` pair is a project-wide
/// safe wire rename, so the emitter also rewrites the module import/export wire
/// name (collapsing the alias).
pub(crate) fn flag_wire_safe_export_renames(
    program: &EnrichedProgram,
    externalized_packages: &BTreeSet<ModuleId>,
    plan: &mut EmitPlan,
) {
    // Modules consumed in a way the emitter's wire pass does NOT rename — a
    // namespace import (`import * as ns`; `ns.o` is a member read) or a
    // re-export barrel (`export { o } from './M'`). Renaming such a module's
    // export wire name would break those consumers, so exclude the whole module.
    let wire_unsafe_modules = modules_consumed_by_namespace_or_reexport(program, plan);
    // Collapsing module M's `export { s as o }` to `export { s }` only stays
    // consistent if EVERY importer of `o` from M also resolves its local binding
    // to `s` (so its `import { o … }` collapses in lockstep). An importer that
    // keeps the minified name (no readability rename) or chose a different local
    // name (a competing rename, e.g. `import { o as v }`) would be left importing
    // a name M no longer exports — `No matching export` at bundle time. Exclude
    // those `(module, original)` bindings so M keeps the export alias instead.
    let uncollapsible = bindings_with_uncollapsible_importer(program, plan);
    let pairs = wire_renameable_pairs(
        program,
        externalized_packages,
        &wire_unsafe_modules,
        &uncollapsible,
    );
    if pairs.is_empty() {
        return;
    }
    for file in &mut plan.files {
        for rename in &mut file.readability_renames {
            if pairs.contains(&(rename.original.clone(), rename.renamed.clone())) {
                rename.wire = true;
            }
        }
    }
}

/// Output paths (extension-stripped) of modules that some file namespace-imports
/// or re-exports. Scanning the planned bodies is exact enough: the planner emits
/// cross-module edges as named imports, so any `import * as`/`export … from`
/// against a first-party path is a genuine namespace/re-export consumer.
fn modules_consumed_by_namespace_or_reexport(
    program: &EnrichedProgram,
    plan: &EmitPlan,
) -> BTreeSet<ModuleId> {
    // Map every module's emitted output path (extension-stripped) to its id.
    let mut module_by_path: BTreeMap<String, ModuleId> = BTreeMap::new();
    for module in program.model().modules() {
        if let Some(path) = crate::module_output_path(program, module.id) {
            module_by_path.insert(strip_source_extension(&path), module.id);
        }
    }
    let mut unsafe_modules = BTreeSet::new();
    for file in &plan.files {
        let body = file.body.join("\n");
        for specifier in namespace_or_reexport_specifiers(&body)
            .into_iter()
            .chain(import_then_reexport_specifiers(&body))
        {
            let resolved = resolve_relative_specifier(file.path.as_str(), specifier.as_str());
            if let Some(module_id) = module_by_path.get(&resolved) {
                unsafe_modules.insert(*module_id);
            }
        }
    }
    unsafe_modules
}

/// Relative-import specifiers whose imported bindings are re-exported by the
/// SAME file through a bare `export { … }` (no `from`). The runtime-helper
/// surface emits exactly this `import { X } from './M'` + separate `export { X }`
/// shape, which a single `export { … } from '…'` scan misses. It forms a
/// re-export chain across hops (definer → helper → consumer) the wire pass
/// cannot keep in sync — the consumer imports `X` from the helper, not from the
/// definer, so collapsing the definer's `export { s as X }` to `export { s }`
/// leaves every downstream `X` dangling. Treat the definer module as wire-unsafe.
fn import_then_reexport_specifiers(body: &str) -> Vec<String> {
    // Local binding name -> source specifier, for relative named imports.
    let mut imported_from: BTreeMap<String, String> = BTreeMap::new();
    let mut reexported_locals: BTreeSet<String> = BTreeSet::new();
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("import") && trimmed.contains('{') && trimmed.contains(" from ") {
            if let Some(specifier) = trailing_from_specifier(trimmed)
                && (specifier.starts_with("./") || specifier.starts_with("../"))
            {
                for (_external, local) in named_clause_entries(trimmed) {
                    imported_from.insert(local, specifier.clone());
                }
            }
        } else if trimmed.starts_with("export")
            && trimmed.contains('{')
            && !trimmed.contains(" from ")
        {
            for (local, _external) in named_clause_entries(trimmed) {
                reexported_locals.insert(local);
            }
        }
    }
    reexported_locals
        .iter()
        .filter_map(|local| imported_from.get(local).cloned())
        .collect()
}

/// Parse a single-line `{ … }` named import/export clause into
/// `(name_before_as, name_after_as)` entries. For `a as b` this is `(a, b)`;
/// for a bare `a` it is `(a, a)`. The caller picks the local-binding side:
/// imports bind the right (`b`); a bare re-export's local binding is the left.
fn named_clause_entries(line: &str) -> Vec<(String, String)> {
    let Some(start) = line.find('{') else {
        return Vec::new();
    };
    let Some(rel_end) = line[start + 1..].find('}') else {
        return Vec::new();
    };
    let inner = &line[start + 1..start + 1 + rel_end];
    inner
        .split(',')
        .filter_map(|raw| {
            let item = raw.trim();
            if item.is_empty() {
                return None;
            }
            Some(match item.split_once(" as ") {
                Some((left, right)) => (left.trim().to_string(), right.trim().to_string()),
                None => (item.to_string(), item.to_string()),
            })
        })
        .collect()
}

/// Relative-import specifiers used in `import * as … from '…'`,
/// `export { … } from '…'`, or `export * from '…'` statements in `body`.
fn namespace_or_reexport_specifiers(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim_start();
        let is_namespace_import = trimmed.starts_with("import")
            && trimmed.contains("* as ")
            && trimmed.contains(" from ");
        let is_reexport = trimmed.starts_with("export") && trimmed.contains(" from ");
        if (is_namespace_import || is_reexport)
            && let Some(specifier) = trailing_from_specifier(trimmed)
            && (specifier.starts_with("./") || specifier.starts_with("../"))
        {
            out.push(specifier);
        }
    }
    out
}

/// The specifier string of the LAST `from '…'`/`from "…"` clause on a line.
fn trailing_from_specifier(line: &str) -> Option<String> {
    let after = line.rsplit_once(" from ")?.1.trim();
    let bytes = after.as_bytes();
    let quote = *bytes.first()?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    let rest = &after[1..];
    let end = rest.find(quote as char)?;
    Some(rest[..end].to_string())
}

/// Resolve a relative `specifier` against the directory of `from_path`, dropping
/// the source extension, to match the extension-stripped module output paths.
fn resolve_relative_specifier(from_path: &str, specifier: &str) -> String {
    let mut segments: Vec<&str> = from_path.split('/').collect();
    segments.pop(); // drop the file name, keep the directory
    let specifier_path = strip_source_extension(specifier);
    for part in specifier_path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    segments.join("/")
}

fn strip_source_extension(path: &str) -> String {
    for extension in [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"] {
        if let Some(stripped) = path.strip_suffix(extension) {
            return stripped.to_string();
        }
    }
    path.to_string()
}

/// The `(original_wire_name, semantic_name)` pairs safe to rename project-wide.
///
/// A binding `o` exported by application module `M` with semantic name `s`
/// qualifies only when ALL of:
/// - `M` is first-party application code (not an externalized package);
/// - `M` is not namespace-imported or re-exported anywhere
///   (`wire_unsafe_modules`) — the only consumer forms the emitter's wire pass
///   cannot keep in sync;
/// - `s` is globally unique among all assigned semantic names — no other binding
///   anywhere resolves to `s`, so flagging by `(o, s)` pair can never touch a
///   different binding, and (with the next rule) the importer rename to `s`
///   can never collide, so it always fires for every direct named importer;
/// - `s` does not collide with any binding's original (minified) name anywhere.
///
/// Minified names are reused across modules, so "exported by exactly one module"
/// would reject almost everything — and would be the WRONG test, since two
/// modules exporting the same minified name are distinct bindings, not a
/// re-export. Re-export is detected structurally via `wire_unsafe_modules`
/// instead.
/// `(defining_module, original)` bindings that have at least one named importer
/// whose local binding does NOT resolve to the binding's semantic name, so the
/// emitter's wire pass cannot collapse that importer's `import { original … }`
/// in lockstep with a collapsed export. Computed from the planned bodies (which
/// still carry minified names) plus each importer file's readability renames.
fn bindings_with_uncollapsible_importer(
    program: &EnrichedProgram,
    plan: &EmitPlan,
) -> BTreeSet<(ModuleId, BindingName)> {
    let mut module_by_path: BTreeMap<String, ModuleId> = BTreeMap::new();
    for module in program.model().modules() {
        if let Some(path) = crate::module_output_path(program, module.id) {
            module_by_path.insert(strip_source_extension(&path), module.id);
        }
    }
    let mut uncollapsible = BTreeSet::new();
    for file in &plan.files {
        // This file's readability renames: minified original -> chosen local
        // name(s). A binding can carry more than one candidate rename (its own
        // readability choice AND a propagated owner-semantic rename); which one
        // the emitter actually applies is order-dependent, so treat ANY target
        // that differs from the semantic name as a reason the import may not
        // collapse — favouring the safe (keep-alias) outcome.
        let mut renames: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for rename in &file.readability_renames {
            renames
                .entry(rename.original.as_str())
                .or_default()
                .push(rename.renamed.as_str());
        }
        let body = file.body.join("\n");
        for line in body.lines() {
            let trimmed = line.trim_start();
            if !(trimmed.starts_with("import")
                && trimmed.contains('{')
                && trimmed.contains(" from "))
            {
                continue;
            }
            let Some(specifier) = trailing_from_specifier(trimmed) else {
                continue;
            };
            if !(specifier.starts_with("./") || specifier.starts_with("../")) {
                continue;
            }
            let resolved = resolve_relative_specifier(file.path.as_str(), specifier.as_str());
            let Some(&module_id) = module_by_path.get(&resolved) else {
                continue;
            };
            // For an import clause `{ imported as local }`, `named_clause_entries`
            // returns `(imported, local)`.
            for (imported, body_local) in named_clause_entries(trimmed) {
                let Some(semantic) = program
                    .semantic_names()
                    .binding_name(module_id, imported.as_str())
                else {
                    continue;
                };
                if semantic.as_str() == imported.as_str() {
                    continue;
                }
                // The import collapses only if its effective local is exactly the
                // semantic name. With no readability rename, that is the name in
                // the body; with renames, EVERY candidate target must equal it.
                let collapses = match renames.get(imported.as_str()) {
                    Some(targets) => targets.iter().all(|t| *t == semantic.as_str()),
                    None => body_local.as_str() == semantic.as_str(),
                };
                if !collapses {
                    uncollapsible.insert((module_id, BindingName::new(imported)));
                }
            }
        }
    }
    uncollapsible
}

fn wire_renameable_pairs(
    program: &EnrichedProgram,
    externalized_packages: &BTreeSet<ModuleId>,
    wire_unsafe_modules: &BTreeSet<ModuleId>,
    uncollapsible: &BTreeSet<(ModuleId, BindingName)>,
) -> BTreeSet<(BindingName, BindingName)> {
    let model = program.model();
    let graph = model.graph();

    // Global semantic-name multiplicity and the set of every original binding
    // name (to reject collisions with a minified name in any consumer).
    let mut semantic_count: BTreeMap<BindingName, usize> = BTreeMap::new();
    let mut all_originals: BTreeSet<BindingName> = BTreeSet::new();
    for module in model.modules() {
        for original in graph.definitions_for(module.id) {
            if let Some(semantic) = program
                .semantic_names()
                .binding_name(module.id, original.as_str())
            {
                *semantic_count.entry(semantic.clone()).or_default() += 1;
            }
            all_originals.insert(original);
        }
    }

    let mut pairs = BTreeSet::new();
    for module in model.modules() {
        if module.kind != ModuleKind::Application
            || externalized_packages.contains(&module.id)
            || wire_unsafe_modules.contains(&module.id)
        {
            continue;
        }
        // Iterate definitions, not `exports_for`: scope-hoisted split modules
        // carry no recorded export facts (`exports_for` is empty), yet their
        // bindings ARE emitted as `export { … }`. A flagged binding that turns
        // out NOT to be exported simply has no import/export specifier for the
        // wire pass to touch, so it is a harmless no-op; global uniqueness of
        // the semantic name keeps the pass from collapsing any other binding.
        for original in graph.definitions_for(module.id) {
            let Some(semantic) = program
                .semantic_names()
                .binding_name(module.id, original.as_str())
            else {
                continue;
            };
            if semantic == &original {
                continue;
            }
            // An importer that will not collapse `original` to `semantic` (kept
            // the minified name, or chose a competing local name) would dangle if
            // M dropped the wire name. Keep the export alias for this binding.
            if uncollapsible.contains(&(module.id, original.clone())) {
                continue;
            }
            if semantic_count.get(semantic).copied() != Some(1) {
                continue;
            }
            if all_originals.contains(semantic) {
                continue;
            }
            pairs.insert((original.clone(), semantic.clone()));
        }
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_specifiers_to_extension_stripped_paths() {
        assert_eq!(
            resolve_relative_specifier("modules/482-x.ts", "./247-y.js"),
            "modules/247-y"
        );
        assert_eq!(
            resolve_relative_specifier("modules/a/b.ts", "../247-y.js"),
            "modules/247-y"
        );
        assert_eq!(
            resolve_relative_specifier("modules/feature/x.ts", "./sub/y.js"),
            "modules/feature/sub/y"
        );
    }

    #[test]
    fn named_clause_entries_splits_aliases_either_direction() {
        assert_eq!(
            named_clause_entries("import { a, b as c } from './m.js';"),
            vec![
                ("a".to_string(), "a".to_string()),
                ("b".to_string(), "c".to_string()),
            ]
        );
        assert_eq!(named_clause_entries("var x = 1;"), Vec::new());
    }

    #[test]
    fn import_then_reexport_detects_chain_but_not_import_only_or_reexport_from() {
        // Imports X from m, re-exports X via a bare `export { X }` → m is unsafe.
        let chain = "import { X, Y } from './m.js';\n\
                     export { X };\n";
        assert_eq!(
            import_then_reexport_specifiers(chain),
            vec!["./m.js".to_string()],
            "import-then-reexport chain"
        );
        // Imports but does NOT re-export → not a chain.
        let import_only = "import { X } from './m.js';\nvar v = X();\nexport { v };\n";
        assert!(
            import_then_reexport_specifiers(import_only).is_empty(),
            "import-only is not a chain"
        );
        // A bare re-export of a LOCAL binding (not imported) → not a chain.
        let local_export = "var local = 1;\nexport { local };\n";
        assert!(
            import_then_reexport_specifiers(local_export).is_empty(),
            "local export is not a chain"
        );
    }

    #[test]
    fn detects_namespace_imports_and_reexports_but_not_named_imports() {
        let body = "import { Cb, Dx } from './a.js';\n\
                    import * as ns from './b.js';\n\
                    export { Ee } from './c.js';\n\
                    export * from './d.js';\n\
                    var local = 1;\n\
                    export { local };\n";
        let specs = namespace_or_reexport_specifiers(body);
        assert!(
            specs.contains(&"./b.js".to_string()),
            "namespace import: {specs:?}"
        );
        assert!(
            specs.contains(&"./c.js".to_string()),
            "re-export: {specs:?}"
        );
        assert!(
            specs.contains(&"./d.js".to_string()),
            "star re-export: {specs:?}"
        );
        assert!(
            !specs.contains(&"./a.js".to_string()),
            "named import excluded: {specs:?}"
        );
        assert_eq!(specs.len(), 3, "only the unsafe forms: {specs:?}");
    }

    #[test]
    fn trailing_specifier_reads_the_quoted_source() {
        assert_eq!(
            trailing_from_specifier("export { x } from './m.js';").as_deref(),
            Some("./m.js")
        );
        assert_eq!(
            trailing_from_specifier("import * as ns from \"./m.js\";").as_deref(),
            Some("./m.js")
        );
        assert_eq!(trailing_from_specifier("export { local };"), None);
    }
}
