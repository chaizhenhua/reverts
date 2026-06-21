//! Completion pass: add referenced-but-unbound bindings to an EXISTING import.
//!
//! The runtime-reader-cluster migration can route a binding that a module uses
//! in call position (e.g. the esbuild `__esm` init thunk `D48` in the
//! `await import()` lowering `Promise.resolve().then(() => (D48(), Ud8))`) into a
//! cycle-avoidance bucket whose emission does not bind it locally, while the
//! namespace `Ud8` from the SAME module is imported directly. The result is a
//! module that references `D48()` but never imports `D48` → `ReferenceError` at
//! runtime (see memory: print-path dropped-import root cause).
//!
//! This pass repairs exactly that shape and ONLY that shape: for each emitted
//! module, a value/call-referenced identifier that is (a) not locally bound,
//! (b) not already imported, (c) not a runtime/JS global, and (d) exported by
//! EXACTLY ONE module the file ALREADY imports from, is added to that existing
//! import clause. Because it only augments an import statement that already
//! exists, it introduces NO new module-to-module edge and therefore cannot
//! create an ESM init cycle — the precise constraint the migration was guarding.

use std::collections::{BTreeMap, BTreeSet};

use crate::EmitPlan;
use crate::local_bindings::local_bindings_in_source;
use crate::plan::PlannedFile;
use crate::runtime_globals::is_runtime_global_identifier;
use crate::runtime_source_scan::value_identifiers_in_source;
use crate::statement_parsers::parse_generated_named_import_specifiers;

/// Returns the number of bindings added to existing imports.
pub(crate) fn complete_referenced_module_imports(plan: &mut EmitPlan) -> usize {
    let exports_by_path: BTreeMap<String, BTreeSet<String>> = plan
        .files
        .iter()
        .map(|file| (file.path.clone(), module_exported_names(file)))
        .collect();

    let mut added = 0usize;
    for file in &mut plan.files {
        let body = file.body.join("\n");

        // Parse existing named imports: (line, local names, resolved target path).
        let mut imports: Vec<(String, BTreeSet<String>, Option<String>)> = Vec::new();
        let mut imported_locals = BTreeSet::<String>::new();
        for line in body.lines() {
            let trimmed = line.trim();
            let Some((specifiers, specifier)) = parse_generated_named_import_specifiers(trimmed)
            else {
                continue;
            };
            let locals: BTreeSet<String> = specifiers
                .iter()
                .map(|spec| spec.local.as_str().to_string())
                .collect();
            imported_locals.extend(locals.iter().cloned());
            let target = resolve_relative_specifier(&file.path, &specifier);
            imports.push((trimmed.to_string(), locals, target));
        }
        if imports.is_empty() {
            continue;
        }

        let local_bindings = local_bindings_in_source(&body);
        let referenced = value_identifiers_in_source(&body);

        // For each unbound referenced name, find the unique already-imported
        // module that exports it and add it to that import.
        let mut planned_for_line: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for name in &referenced {
            if imported_locals.contains(name)
                || local_bindings.contains(name)
                || is_runtime_global_identifier(name)
            {
                continue;
            }
            let mut matches = imports.iter().filter(|(_, locals, target)| {
                !locals.contains(name)
                    && target
                        .as_deref()
                        .and_then(|path| exports_by_path.get(path))
                        .is_some_and(|exports| exports.contains(name))
            });
            let Some((line, _, _)) = matches.next() else {
                continue;
            };
            if matches.next().is_some() {
                // Ambiguous: exported by more than one already-imported module.
                continue;
            }
            planned_for_line
                .entry(line.clone())
                .or_default()
                .insert(name.clone());
        }
        if planned_for_line.is_empty() {
            continue;
        }

        let mut new_body = body.clone();
        for (line, names) in planned_for_line {
            let Some(rebuilt) = add_names_to_import_line(&line, &names) else {
                continue;
            };
            new_body = new_body.replacen(&line, &rebuilt, 1);
            added += names.len();
        }
        file.body = vec![new_body];
    }
    added
}

/// Every name a module makes importable: structured planned exports plus any
/// `export { … }` / `export <decl> NAME` in its body (external names, post-`as`).
pub(crate) fn module_exported_names(file: &PlannedFile) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = file
        .exports
        .iter()
        .map(|export| export.binding.as_str().to_string())
        .collect();
    let body = file.body.join("\n");
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("export { ")
            && let Some((names, _)) = rest.split_once(" }")
        {
            for part in names.split(',') {
                let part = part.trim();
                if part.is_empty() {
                    continue;
                }
                // `A` → A; `A as B` → B (the external/exported name).
                let exported = part.rsplit(" as ").next().unwrap_or(part).trim();
                if is_identifier(exported) {
                    out.insert(exported.to_string());
                }
            }
        }
        for kw in [
            "export var ",
            "export const ",
            "export let ",
            "export function ",
            "export async function ",
            "export class ",
        ] {
            if let Some(rest) = trimmed.strip_prefix(kw) {
                let name: String = rest
                    .chars()
                    .take_while(|ch| ch.is_alphanumeric() || *ch == '_' || *ch == '$')
                    .collect();
                if is_identifier(&name) {
                    out.insert(name);
                }
            }
        }
    }
    out
}

/// Insert `names` into the clause of a generated `import { … } from '…';` line.
fn add_names_to_import_line(line: &str, names: &BTreeSet<String>) -> Option<String> {
    let rest = line.trim().strip_prefix("import { ")?;
    let (existing, tail) = rest.split_once(" } from '")?;
    let specifier = tail.strip_suffix("';")?;
    let mut all: BTreeSet<String> = existing
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect();
    all.extend(names.iter().cloned());
    let joined = all.into_iter().collect::<Vec<_>>().join(", ");
    Some(format!("import {{ {joined} }} from '{specifier}';"))
}

pub(crate) fn resolve_relative_specifier(from_file: &str, specifier: &str) -> Option<String> {
    if !(specifier.starts_with("./") || specifier.starts_with("../")) {
        return None;
    }
    let dir = from_file.rsplit_once('/').map_or("", |(dir, _)| dir);
    let mut parts: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    for segment in specifier.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    let mut path = parts.join("/");
    if let Some(stripped) = path.strip_suffix(".js") {
        path = format!("{stripped}.ts");
    } else if !path.ends_with(".ts") {
        path.push_str(".ts");
    }
    Some(path)
}

fn is_identifier(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '$')
        && !name.chars().next().is_some_and(|ch| ch.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::PlannedFile;

    #[test]
    fn adds_referenced_unbound_export_to_existing_import() {
        // Mirrors the esbuild `await import()` lowering: the consumer imports the
        // namespace `Ud8` from `./dep.js` but references the init thunk `D48()`
        // without importing it. `dep` exports both. The pass adds `D48` to the
        // existing import (no new module edge).
        let mut plan = EmitPlan::default();
        let mut consumer = PlannedFile::new("modules/consumer.ts");
        consumer.push_source(
            "import { Ud8 } from './dep.js';\nfunction run() { return (D48(), Ud8); }",
        );
        plan.push_file(consumer);
        let mut dep = PlannedFile::new("modules/dep.ts");
        dep.push_source("var D48 = () => {};\nvar Ud8 = {};\nexport { D48, Ud8 };");
        plan.push_file(dep);

        let added = complete_referenced_module_imports(&mut plan);
        assert_eq!(added, 1);
        assert!(
            plan.files[0]
                .body
                .join("\n")
                .contains("import { D48, Ud8 } from './dep.js';"),
            "{}",
            plan.files[0].body.join("\n")
        );
    }

    #[test]
    fn does_not_synthesize_a_new_import_when_owner_not_already_imported() {
        // Cycle-safety: with no existing import from `dep`, the pass must NOT add
        // a new import edge (which could create an ESM init cycle).
        let mut plan = EmitPlan::default();
        let mut consumer = PlannedFile::new("modules/consumer.ts");
        consumer.push_source("function run() { return D48(); }");
        plan.push_file(consumer);
        let mut dep = PlannedFile::new("modules/dep.ts");
        dep.push_source("var D48 = () => {};\nexport { D48 };");
        plan.push_file(dep);

        assert_eq!(complete_referenced_module_imports(&mut plan), 0);
    }

    #[test]
    fn does_not_add_when_binding_not_exported_by_owner() {
        // The owner is imported, but does not export the referenced binding —
        // nothing to import (this is the separate "thunk not emitted" bug class).
        let mut plan = EmitPlan::default();
        let mut consumer = PlannedFile::new("modules/consumer.ts");
        consumer.push_source(
            "import { fn8 } from './dep.js';\nfunction run() { return (Zn8(), fn8); }",
        );
        plan.push_file(consumer);
        let mut dep = PlannedFile::new("modules/dep.ts");
        dep.push_source("var fn8 = {};\nexport { fn8 };");
        plan.push_file(dep);

        assert_eq!(complete_referenced_module_imports(&mut plan), 0);
    }
}
