//! Completion pass: a name a consumer imports from a sibling module must be
//! exported by that module.
//!
//! The recovered export surface of a minified esbuild module is sometimes
//! narrower than what sibling modules actually import from it — esbuild exposes
//! some bindings purely through scope hoisting, so neither the module's
//! `__export` map nor the def-use graph records them, yet a consumer's recovered
//! body still imports them by name. The emitted ESM then has a consumer
//! `import { X } from './m.js'` with no matching export in `m` → a load-time
//! `SyntaxError: … does not provide an export named 'X'`.
//!
//! This pass reconciles the two sides generically, working from the EMITTED
//! import statements (ground truth of what is needed) rather than the graph
//! (which has the gaps). For every relative `import { … } from './sibling.js'`,
//! the sibling must export those names. A still-missing name is added as:
//!
//! - `export { X } from '<pkg>'` when the module is an externalized package
//!   re-export (its body holds `import * as ns from '<bare-pkg>'`) — the name is
//!   sourced straight from the bare package the module already stands in for; or
//! - `export { X }` when the module defines `X` locally.
//!
//! A name the module can neither re-export nor define is left untouched (a
//! genuine recovery gap, not something this pass can invent). Nothing is ever
//! removed, and a name already exported is skipped, so the pass cannot create a
//! duplicate export.

use std::collections::{BTreeMap, BTreeSet};

use reverts_package::PackageResolution;

use crate::EmitPlan;
use crate::complete_referenced_imports::{module_exported_names, resolve_relative_specifier};
use crate::local_bindings::local_bindings_in_source;
use crate::plan::PlannedFile;
use crate::statement_parsers::parse_generated_named_import_specifiers;

/// Returns the number of export names added across all files.
pub(crate) fn complete_cross_module_exports(plan: &mut EmitPlan) -> usize {
    // 1. For each module path, the union of names some sibling imports from it.
    let mut required_by_path: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for file in &plan.files {
        let body = file.body.join("\n");
        // Imports are emitted one-per-line in most modules but concatenated onto
        // a single line in the entrypoint island, so scan by STATEMENT (each
        // `import …;`) rather than by line.
        for statement in import_statements(&body) {
            let Some((specifiers, specifier)) = parse_generated_named_import_specifiers(&statement)
            else {
                continue;
            };
            let Some(target) = resolve_relative_specifier(&file.path, &specifier) else {
                continue;
            };
            let entry = required_by_path.entry(target).or_default();
            for spec in &specifiers {
                // The IMPORTED (external) name is what the target must export,
                // not the consumer's local alias.
                entry.insert(spec.imported.as_str().to_string());
            }
        }
    }
    if required_by_path.is_empty() {
        return 0;
    }

    // 2. Add the missing exports each target can legitimately provide.
    let mut added = 0usize;
    for file in &mut plan.files {
        let Some(required) = required_by_path.get(&file.path) else {
            continue;
        };
        let existing = module_exported_names(file);
        let missing: BTreeSet<String> = required.difference(&existing).cloned().collect();
        if missing.is_empty() {
            continue;
        }

        let reexport_package = externalized_package(file);
        let local_bindings = local_bindings_in_source(&file.body.join("\n"));

        let mut local_exports = BTreeSet::<String>::new();
        let mut reexports = BTreeSet::<String>::new();
        for name in missing {
            if local_bindings.contains(&name) {
                local_exports.insert(name);
            } else if reexport_package.is_some() {
                reexports.insert(name);
            }
            // Otherwise the module can neither define nor re-export it — a
            // genuine recovery gap left for upstream recovery to close.
        }

        if !local_exports.is_empty() {
            file.push_source(format!("export {{ {} }};", join_names(&local_exports)));
            added += local_exports.len();
        }
        if let Some(package) = reexport_package
            && !reexports.is_empty()
        {
            file.push_source(format!(
                "export {{ {} }} from '{package}';",
                join_names(&reexports)
            ));
            added += reexports.len();
        }
    }
    added
}

/// Yield each `import …;` statement from a body, normalised to end with `';`.
/// Modules emit one import per line, but the entrypoint island concatenates them
/// onto a single line, so a line-based scan would mis-parse the whole run as one
/// statement. Splitting on the statement terminator handles both.
fn import_statements(body: &str) -> Vec<String> {
    let mut statements = Vec::new();
    for segment in body.split(';') {
        // Take from the `import ` keyword to the segment end, so a leading
        // comment or preceding token (`// @ts-nocheck\nimport …`) doesn't hide
        // the statement. Non-conforming captures are rejected by the parser.
        if let Some(pos) = segment.find("import ") {
            let statement = segment[pos..].trim();
            statements.push(format!("{statement};"));
        }
    }
    statements
}

fn join_names(names: &BTreeSet<String>) -> String {
    names.iter().cloned().collect::<Vec<_>>().join(", ")
}

/// The single bare package an externalized module stands in for, so any name it
/// must export can be re-exported straight from that package. The namespace
/// import is usually a structured `PlannedImport` (`import * as ns from 'pkg'`,
/// added via `add_import`), so read it there; fall back to a body-text scan.
/// Returns `None` when there is no external import or more than one distinct
/// package (ambiguous — cannot know which provides the missing name).
fn externalized_package(file: &PlannedFile) -> Option<String> {
    let mut packages = BTreeSet::<String>::new();
    for import in &file.imports {
        if let PackageResolution::External { specifier, .. } = &import.resolution {
            packages.insert(specifier.clone());
        }
    }
    if let Some(package) = externalized_namespace_package_in_body(&file.body.join("\n")) {
        packages.insert(package);
    }
    (packages.len() == 1)
        .then(|| packages.into_iter().next())
        .flatten()
}

/// Body-text fallback: `import * as <ns> from '<bare-pkg>';` (not a relative path).
fn externalized_namespace_package_in_body(body: &str) -> Option<String> {
    for line in body.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("import * as ") else {
            continue;
        };
        let Some((_namespace, tail)) = rest.split_once(" from ") else {
            continue;
        };
        let tail = tail.trim().trim_end_matches(';').trim();
        let specifier = tail
            .strip_prefix('\'')
            .and_then(|s| s.strip_suffix('\''))
            .or_else(|| tail.strip_prefix('"').and_then(|s| s.strip_suffix('"')))?;
        if specifier.starts_with("./") || specifier.starts_with("../") || specifier.is_empty() {
            continue;
        }
        return Some(specifier.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::PlannedFile;

    #[test]
    fn reexports_missing_name_from_externalized_package() {
        // The consumer imports `MessageEvent` from an externalized sub-module
        // whose recovered body only destructured `Event`/`EventTarget` from the
        // bare package. The missing name must be re-exported straight from `ws`.
        let mut plan = EmitPlan::default();
        let mut consumer = PlannedFile::new("modules/entrypoint.ts");
        consumer.push_source("import { MessageEvent } from './ws/event-target.js';");
        plan.push_file(consumer);
        let mut target = PlannedFile::new("modules/ws/event-target.ts");
        target.push_source("import * as external_ws from 'ws';\nconst { Event } = external_ws;");
        plan.push_file(target);

        let added = complete_cross_module_exports(&mut plan);
        assert_eq!(added, 1);
        assert!(
            plan.files[1]
                .body
                .join("\n")
                .contains("export { MessageEvent } from 'ws';"),
            "{}",
            plan.files[1].body.join("\n")
        );
    }

    #[test]
    fn exports_missing_name_defined_locally() {
        // The consumer imports `si`, which the target defines locally but never
        // exported — add a plain `export { si }`.
        let mut plan = EmitPlan::default();
        let mut consumer = PlannedFile::new("modules/a.ts");
        consumer.push_source("import { si } from './b.js';");
        plan.push_file(consumer);
        let mut target = PlannedFile::new("modules/b.ts");
        target.push_source("function si(t) { return t; }\nexport { st };\nconst st = 1;");
        plan.push_file(target);

        let added = complete_cross_module_exports(&mut plan);
        assert_eq!(added, 1);
        assert!(
            plan.files[1].body.join("\n").contains("export { si };"),
            "{}",
            plan.files[1].body.join("\n")
        );
    }

    #[test]
    fn skips_already_exported_and_unprovidable_names() {
        // `keep` is already exported (no duplicate); `ghost` is neither defined
        // nor re-exportable (no bare namespace import) — left untouched.
        let mut plan = EmitPlan::default();
        let mut consumer = PlannedFile::new("modules/a.ts");
        consumer.push_source("import { keep, ghost } from './b.js';");
        plan.push_file(consumer);
        let mut target = PlannedFile::new("modules/b.ts");
        target.push_source("const keep = 1;\nexport { keep };");
        plan.push_file(target);

        assert_eq!(complete_cross_module_exports(&mut plan), 0);
        assert!(!plan.files[1].body.join("\n").contains("ghost"));
    }
}
