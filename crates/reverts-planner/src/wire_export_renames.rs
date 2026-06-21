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
    let pairs = wire_renameable_pairs(program, externalized_packages, &wire_unsafe_modules);
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
        for specifier in namespace_or_reexport_specifiers(&body) {
            let resolved = resolve_relative_specifier(file.path.as_str(), specifier.as_str());
            if let Some(module_id) = module_by_path.get(&resolved) {
                unsafe_modules.insert(*module_id);
            }
        }
    }
    unsafe_modules
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
fn wire_renameable_pairs(
    program: &EnrichedProgram,
    externalized_packages: &BTreeSet<ModuleId>,
    wire_unsafe_modules: &BTreeSet<ModuleId>,
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
        assert!(specs.contains(&"./c.js".to_string()), "re-export: {specs:?}");
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
