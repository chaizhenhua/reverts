//! Collapse dead wire-name aliases in island-cluster exports (post-emit).
//!
//! Island clusters emit `export { semanticLocal as wireName };` at their file
//! boundary so cross-file links keep using the minified wire name. The planner's
//! `wire_export_renames` pass only collapses these for real model modules, so
//! island clusters — which carry ~99% of the alias "wall" — keep them. The alias
//! text itself only exists AFTER the emitter applies the binding renames, so this
//! runs on the emitted project, not the plan.
//!
//! Scope is the PROVABLY-SAFE subset: an alias whose wire name no file imports
//! (named import), on a cluster nothing namespace-imports or re-exports, and
//! whose local is not otherwise exported by the same clause. Nothing outside the
//! file can then reference `wireName`, so `export { local as wireName }` →
//! `export { local }` changes no consumer. Consumed aliases (~90% of the wall)
//! need a full lockstep export+import rename and are left to a dedicated pass.

use std::collections::BTreeSet;

use reverts_emitter::EmittedFile;

const ISLAND_DIR: &str = "modules/island/";

pub(crate) fn collapse_dead_island_wire_aliases(files: &mut [EmittedFile]) {
    let referenced = referenced_names(files);
    for file in files.iter_mut() {
        if !file.path.starts_with(ISLAND_DIR) || referenced.namespace_consumed.contains(&file.path)
        {
            continue;
        }
        if !file.source.contains("export {") {
            continue;
        }
        file.source = collapse_source(&file.source, &referenced.named_imports);
    }
}

fn collapse_source(source: &str, referenced_wires: &BTreeSet<String>) -> String {
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
        // Names this clause will export AFTER any collapse, so a drop can never
        // duplicate another export name in the same clause.
        let exported_after: Vec<&str> = inner
            .split(',')
            .filter_map(|raw| {
                let item = raw.trim();
                (!item.is_empty()).then(|| match item.split_once(" as ") {
                    Some((local, wire)) => {
                        if referenced_wires.contains(wire.trim()) {
                            wire.trim()
                        } else {
                            local.trim()
                        }
                    }
                    None => item,
                })
            })
            .collect();
        let mut changed = false;
        let rebuilt = inner
            .split(',')
            .filter_map(|raw| {
                let item = raw.trim();
                if item.is_empty() {
                    return None;
                }
                let Some((local, wire)) = item.split_once(" as ") else {
                    return Some(item.to_string());
                };
                let (local, wire) = (local.trim(), wire.trim());
                if !referenced_wires.contains(wire)
                    && exported_after.iter().filter(|n| **n == local).count() == 1
                {
                    changed = true;
                    Some(local.to_string())
                } else {
                    Some(format!("{local} as {wire}"))
                }
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

struct Referenced {
    named_imports: BTreeSet<String>,
    namespace_consumed: BTreeSet<String>,
}

fn referenced_names(files: &[EmittedFile]) -> Referenced {
    let mut named_imports = BTreeSet::<String>::new();
    let mut namespace_consumed = BTreeSet::<String>::new();
    let island_paths: Vec<(String, String)> = files
        .iter()
        .filter(|f| f.path.starts_with(ISLAND_DIR))
        .map(|f| (strip_ts_ext(&f.path), f.path.clone()))
        .collect();
    for file in files {
        for line in file.source.lines() {
            let trimmed = line.trim_start();
            if let Some(names) = named_import_clause(trimmed) {
                for name in names {
                    named_imports.insert(name);
                }
                continue;
            }
            let is_namespace_import = trimmed.starts_with("import")
                && trimmed.contains("* as ")
                && trimmed.contains(" from ");
            let is_reexport = trimmed.starts_with("export") && trimmed.contains(" from ");
            if (is_namespace_import || is_reexport)
                && let Some(spec) = trailing_from_specifier(trimmed)
                && let Some(target) = resolve_island_target(&file.path, &spec, &island_paths)
            {
                namespace_consumed.insert(target);
            }
        }
    }
    Referenced {
        named_imports,
        namespace_consumed,
    }
}

/// The imported (left-of-`as`) names of an `import { a, b as c } from '…';` line.
fn named_import_clause(line: &str) -> Option<Vec<String>> {
    let rest = line.strip_prefix("import { ")?;
    let (names, _) = rest.split_once(" } from ")?;
    Some(
        names
            .split(',')
            .filter_map(|raw| {
                let name = raw.split(" as ").next().unwrap_or(raw).trim();
                (!name.is_empty()).then(|| name.to_string())
            })
            .collect(),
    )
}

fn resolve_island_target(
    from_path: &str,
    specifier: &str,
    island_paths: &[(String, String)],
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
    let resolved = segments.join("/");
    island_paths
        .iter()
        .find(|(stripped, _)| *stripped == resolved)
        .map(|(_, path)| path.clone())
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
    fn drops_dead_wire_alias() {
        let mut files = vec![f(
            "modules/island/auth/oauth.ts",
            "class InvalidRequestError {}\nexport { InvalidRequestError as TFA };",
        )];
        collapse_dead_island_wire_aliases(&mut files);
        assert!(
            files[0].source.contains("export { InvalidRequestError };"),
            "{}",
            files[0].source
        );
    }

    #[test]
    fn keeps_consumed_wire_alias() {
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
        collapse_dead_island_wire_aliases(&mut files);
        assert!(
            files[0].source.contains("InvalidRequestError as TFA"),
            "{}",
            files[0].source
        );
    }

    #[test]
    fn keeps_alias_for_namespace_consumed_cluster() {
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
        collapse_dead_island_wire_aliases(&mut files);
        assert!(
            files[0].source.contains("InvalidRequestError as TFA"),
            "{}",
            files[0].source
        );
    }

    #[test]
    fn mixed_clause_collapses_only_dead() {
        let mut files = vec![
            f(
                "modules/island/auth/oauth.ts",
                "export { A as aWire, B as bWire };",
            ),
            f(
                "modules/island/agent/x.ts",
                "import { bWire } from '../auth/oauth.js';",
            ),
        ];
        collapse_dead_island_wire_aliases(&mut files);
        let s = &files[0].source;
        assert!(s.contains("B as bWire"), "live kept: {s}");
        assert!(
            s.contains("{ A,") || s.contains(", A "),
            "dead collapsed: {s}"
        );
    }

    #[test]
    fn leaves_non_island_untouched() {
        let mut files = vec![f("auth/oauth-constants.ts", "export { X as Y };")];
        collapse_dead_island_wire_aliases(&mut files);
        assert!(files[0].source.contains("X as Y"));
    }
}
