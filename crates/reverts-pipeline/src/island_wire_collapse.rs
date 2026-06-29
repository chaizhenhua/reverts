//! Collapse wire-name aliases in island-cluster exports (post-emit, lockstep).
//!
//! Island clusters emit `export { semanticLocal as wireName };` at their file
//! boundary so cross-file links keep using the minified wire name. The planner's
//! `wire_export_renames` pass only collapses these for real model modules, so
//! island clusters — ~99% of the alias "wall" — keep them. The alias text only
//! exists after the emitter applies the binding renames, so this runs on the
//! emitted project, not the plan.
//!
//! For each cluster export `semanticLocal as wireName` whose `semanticLocal` is
//! unique within that cluster's export clause and whose cluster is not
//! namespace-imported / re-exported, this rewrites IN LOCKSTEP:
//!   - the cluster export `Local as wire` → `Local`, and
//!   - every importer `import { … wire … } from <cluster>` so the imported name
//!     becomes `Local` while the importer's LOCAL binding is preserved
//!     (`wire` → `Local as wire`, `wire as X` → `Local as X`).
//!
//! This is safe with only PER-CLUSTER uniqueness of `Local` (not global): the
//! export side can never duplicate a name, and on the import side the local
//! binding is preserved, so two clusters exporting the same `Local` resolve to
//! distinct local bindings at any shared consumer. Dead aliases (no importer) are
//! the zero-importer case of the same rewrite. Consumer BODIES keep the wire
//! name; renaming those too needs the emitter's scope-aware path and is left to a
//! follow-up. Namespace/re-export consumers reference the export name directly and
//! cannot be kept in lockstep, so their clusters are excluded.

use std::collections::{BTreeMap, BTreeSet};

use reverts_emitter::EmittedFile;

const ISLAND_DIR: &str = "modules/island/";

pub(crate) fn collapse_island_wire_aliases(files: &mut [EmittedFile]) {
    let namespace_consumed = namespace_consumed_islands(files);

    // Pass 1: choose relocatable aliases and rewrite each cluster's export clause.
    // relocations: (cluster_path, wireName) -> semanticLocal
    let mut relocations = BTreeMap::<(String, String), String>::new();
    for file in files.iter_mut() {
        if !file.path.starts_with(ISLAND_DIR) || namespace_consumed.contains(&file.path) {
            continue;
        }
        if !file.source.contains("export {") {
            continue;
        }
        file.source = rewrite_cluster_exports(&file.source, &file.path, &mut relocations);
    }
    if relocations.is_empty() {
        return;
    }

    // Pass 2: repoint every importer of a relocated wire at the cluster's new
    // (semantic) export name, preserving the importer's local binding.
    let island_paths = island_path_index(files);
    for file in files.iter_mut() {
        if !file.source.contains("import {") {
            continue;
        }
        file.source = rewrite_importers(&file.source, &file.path, &relocations, &island_paths);
    }
}

/// Rewrite a cluster's `export { … };` clauses, dropping ` as wire` for each
/// alias whose local is unique in the clause; record `(path, wire) -> local`.
fn rewrite_cluster_exports(
    source: &str,
    cluster_path: &str,
    relocations: &mut BTreeMap<(String, String), String>,
) -> String {
    let mut out = Vec::with_capacity(source.lines().count());
    for line in source.lines() {
        let trimmed = line.trim();
        let Some(inner) = trimmed
            .strip_prefix("export { ")
            .and_then(|rest| rest.strip_suffix(" };"))
        else {
            out.push(line.to_string());
            continue;
        };
        if inner.contains(" from ") {
            out.push(line.to_string());
            continue;
        }
        let entries: Vec<(&str, Option<&str>)> = inner
            .split(',')
            .filter_map(|raw| {
                let item = raw.trim();
                (!item.is_empty()).then(|| match item.split_once(" as ") {
                    Some((local, wire)) => (local.trim(), Some(wire.trim())),
                    None => (item, None),
                })
            })
            .collect();
        // The name each entry will EXPORT after a candidate collapse, to enforce
        // per-clause uniqueness (no drop may duplicate another exported name).
        let mut export_name_count = BTreeMap::<&str, usize>::new();
        for (local, wire) in &entries {
            let exported = match wire {
                Some(_) => *local, // candidate-collapsed name
                None => *local,
            };
            *export_name_count.entry(exported).or_default() += 1;
        }
        let mut changed = false;
        let rebuilt = entries
            .iter()
            .map(|(local, wire)| match wire {
                Some(wire) if export_name_count.get(local).copied().unwrap_or(0) == 1 => {
                    changed = true;
                    relocations.insert(
                        (cluster_path.to_string(), (*wire).to_string()),
                        (*local).to_string(),
                    );
                    (*local).to_string()
                }
                Some(wire) => format!("{local} as {wire}"),
                None => (*local).to_string(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        if changed {
            out.push(format!("export {{ {rebuilt} }};"));
        } else {
            out.push(line.to_string());
        }
    }
    out.join("\n")
}

/// Rewrite `import { … } from '<cluster>';` lines: for each imported wire that a
/// relocation renamed to `local`, change the imported name to `local` and keep
/// the importer's local binding (`wire` → `local as wire`, `wire as X` →
/// `local as X`).
fn rewrite_importers(
    source: &str,
    from_path: &str,
    relocations: &BTreeMap<(String, String), String>,
    island_paths: &BTreeMap<String, String>,
) -> String {
    let mut out = Vec::with_capacity(source.lines().count());
    for line in source.lines() {
        let trimmed = line.trim();
        let Some((names, specifier)) = trimmed
            .strip_prefix("import { ")
            .and_then(|rest| rest.split_once(" } from "))
        else {
            out.push(line.to_string());
            continue;
        };
        let Some(spec) = unquote_trailing(specifier) else {
            out.push(line.to_string());
            continue;
        };
        let Some(cluster) = resolve_island_target(from_path, &spec, island_paths) else {
            out.push(line.to_string());
            continue;
        };
        let mut changed = false;
        let rebuilt = names
            .split(',')
            .filter_map(|raw| {
                let item = raw.trim();
                if item.is_empty() {
                    return None;
                }
                let (imported, local_binding) = match item.split_once(" as ") {
                    Some((imported, local)) => (imported.trim(), Some(local.trim())),
                    None => (item, None),
                };
                if let Some(new_imported) =
                    relocations.get(&(cluster.clone(), imported.to_string()))
                {
                    changed = true;
                    let binding = local_binding.unwrap_or(imported);
                    Some(format!("{new_imported} as {binding}"))
                } else {
                    Some(item.to_string())
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        if changed {
            // Preserve the original quote style of the specifier.
            let quote = specifier.trim().chars().next().unwrap_or('\'');
            out.push(format!("import {{ {rebuilt} }} from {quote}{spec}{quote};"));
        } else {
            out.push(line.to_string());
        }
    }
    out.join("\n")
}

fn island_path_index(files: &[EmittedFile]) -> BTreeMap<String, String> {
    files
        .iter()
        .filter(|f| f.path.starts_with(ISLAND_DIR))
        .map(|f| (strip_ts_ext(&f.path), f.path.clone()))
        .collect()
}

/// Island file paths some file namespace-imports (`import * as`) or re-exports
/// (`export … from`) — excluded because those reference the export name directly.
fn namespace_consumed_islands(files: &[EmittedFile]) -> BTreeSet<String> {
    let island_paths = island_path_index(files);
    let mut consumed = BTreeSet::new();
    for file in files {
        for line in file.source.lines() {
            let trimmed = line.trim_start();
            let is_namespace_import = trimmed.starts_with("import")
                && trimmed.contains("* as ")
                && trimmed.contains(" from ");
            let is_reexport = trimmed.starts_with("export") && trimmed.contains(" from ");
            if (is_namespace_import || is_reexport)
                && let Some(spec) = trailing_from_specifier(trimmed)
                && let Some(target) = resolve_island_target(&file.path, &spec, &island_paths)
            {
                consumed.insert(target);
            }
        }
    }
    consumed
}

fn resolve_island_target(
    from_path: &str,
    specifier: &str,
    island_paths: &BTreeMap<String, String>,
) -> Option<String> {
    if !(specifier.starts_with("./") || specifier.starts_with("../")) {
        return None;
    }
    let mut segments: Vec<&str> = from_path.split('/').collect();
    segments.pop();
    let stem = strip_ts_ext(specifier);
    for part in stem.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    island_paths.get(&segments.join("/")).cloned()
}

fn unquote_trailing(specifier: &str) -> Option<String> {
    let s = specifier
        .trim()
        .strip_suffix(';')
        .unwrap_or(specifier.trim());
    let bytes = s.as_bytes();
    let quote = *bytes.first()?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    let rest = &s[1..];
    let end = rest.find(quote as char)?;
    Some(rest[..end].to_string())
}

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

fn strip_ts_ext(path: &str) -> String {
    for extension in [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"] {
        if let Some(stripped) = path.strip_suffix(extension) {
            return stripped.to_string();
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(path: &str, source: &str) -> EmittedFile {
        EmittedFile {
            path: path.to_string(),
            source: source.to_string(),
        }
    }

    #[test]
    fn dead_alias_collapses_with_no_importer() {
        let mut files = vec![f(
            "modules/island/auth/oauth.ts",
            "export { InvalidRequestError as TFA };",
        )];
        collapse_island_wire_aliases(&mut files);
        assert_eq!(files[0].source, "export { InvalidRequestError };");
    }

    #[test]
    fn consumed_alias_relocates_in_lockstep() {
        let mut files = vec![
            f(
                "modules/island/auth/oauth.ts",
                "export { InvalidRequestError as TFA };",
            ),
            f(
                "modules/island/agent/x.ts",
                "import { TFA } from '../auth/oauth.js';",
            ),
        ];
        collapse_island_wire_aliases(&mut files);
        assert_eq!(files[0].source, "export { InvalidRequestError };");
        assert_eq!(
            files[1].source,
            "import { InvalidRequestError as TFA } from '../auth/oauth.js';"
        );
    }

    #[test]
    fn preserves_consumer_local_alias() {
        let mut files = vec![
            f(
                "modules/island/auth/oauth.ts",
                "export { InvalidRequestError as TFA };",
            ),
            f(
                "modules/island/agent/x.ts",
                "import { TFA as err } from '../auth/oauth.js';",
            ),
        ];
        collapse_island_wire_aliases(&mut files);
        assert_eq!(
            files[1].source,
            "import { InvalidRequestError as err } from '../auth/oauth.js';"
        );
    }

    #[test]
    fn keeps_alias_when_local_not_unique_in_clause() {
        // `Foo` exported under two wire names → collapsing both would duplicate.
        let mut files = vec![f(
            "modules/island/auth/oauth.ts",
            "export { Foo as A, Foo as B };",
        )];
        collapse_island_wire_aliases(&mut files);
        assert_eq!(files[0].source, "export { Foo as A, Foo as B };");
    }

    #[test]
    fn excludes_namespace_consumed_cluster() {
        let mut files = vec![
            f(
                "modules/island/auth/oauth.ts",
                "export { InvalidRequestError as TFA };",
            ),
            f(
                "modules/island/agent/x.ts",
                "import * as oauth from '../auth/oauth.js';",
            ),
        ];
        collapse_island_wire_aliases(&mut files);
        assert_eq!(files[0].source, "export { InvalidRequestError as TFA };");
    }

    #[test]
    fn distinct_clusters_same_semantic_do_not_collide_at_consumer() {
        let mut files = vec![
            f("modules/island/a.ts", "export { Foo as aW };"),
            f("modules/island/b.ts", "export { Foo as bW };"),
            f(
                "modules/island/c.ts",
                "import { aW } from './a.js';\nimport { bW } from './b.js';",
            ),
        ];
        collapse_island_wire_aliases(&mut files);
        assert_eq!(files[0].source, "export { Foo };");
        assert_eq!(files[1].source, "export { Foo };");
        assert!(
            files[2]
                .source
                .contains("import { Foo as aW } from './a.js';")
        );
        assert!(
            files[2]
                .source
                .contains("import { Foo as bW } from './b.js';")
        );
    }

    #[test]
    fn leaves_non_island_untouched() {
        let mut files = vec![f("auth/oauth-constants.ts", "export { X as Y };")];
        collapse_island_wire_aliases(&mut files);
        assert_eq!(files[0].source, "export { X as Y };");
    }
}
