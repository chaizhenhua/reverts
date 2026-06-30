//! Collapse wire-name export aliases (post-emit, lockstep) for island clusters
//! AND the entrypoint hub barrel.
//!
//! Island clusters emit `export { semanticLocal as wireName };` at their file
//! boundary so cross-file links keep using the minified wire name. The planner's
//! `wire_export_renames` pass only collapses these for real model modules, so
//! island clusters — ~99% of the alias "wall" — keep them. The alias text only
//! exists after the emitter applies the binding renames, so this runs on the
//! emitted project, not the plan.
//!
//! The same shape covers `modules/entrypoint.ts`, the star-topology re-export hub
//! that ~hundreds of files import from by wire name. Unlike clusters it exports
//! BARE wire names (`export { $FA }`) at this stage; its real names are recovered
//! only when Pass 3 re-emits it (after a round repoints its own imports from
//! collapsed islands), so the collapse iterates to a fixpoint: round 1 collapses
//! islands and recovers the hub's `Real as wire` exports; round 2 collapses those.
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
//! the zero-importer case of the same rewrite. Namespace/re-export consumers
//! reference the export name directly and cannot be kept in lockstep, so their
//! clusters are excluded.
//!
//! Pass 3 then renames each consumer's LOCAL binding from the wire name to the
//! real (imported) name, scope-awarely, by re-running the emitter's renamer on the
//! file — so `import { Real as wire }` collapses to `import { Real }` and the
//! consumer BODY reads `Real` instead of the minified wire name. This is provably
//! safe: only the local binding + its references change; the IMPORTED name is
//! unchanged, so the cross-file export contract cannot break (a colliding rename
//! is skipped by the renamer, leaving a harmless alias). The real name is taken
//! from the import alias the emitter already recovered (the esbuild export name),
//! NOT the binding-naming channel — the two can disagree, and the export name is
//! the one the cluster actually exports.

use std::collections::{BTreeMap, BTreeSet};

use reverts_emitter::EmittedFile;

pub(crate) fn collapse_island_wire_aliases(files: &mut [EmittedFile]) {
    let namespace_consumed = namespace_consumed_islands(files);
    let island_paths = island_path_index(files);

    // Iterate to a fixpoint. A single round cannot fully collapse the entrypoint
    // hub: at round start the hub still exports bare wire names; only AFTER a round
    // repoints the hub's own imports from collapsed islands (Pass 2) and re-emits
    // the hub (Pass 3) do its `export { $wire }` lines recover to `Real as $wire`.
    // The next round's Pass 1 then sees and collapses them. Two rounds suffice in
    // practice (islands → hub); the cap is a safety bound.
    let mut total = 0usize;
    for _ in 0..3 {
        let round = collapse_round(files, &namespace_consumed, &island_paths);
        total += round;
        if round == 0 {
            break;
        }
    }
    let _ = total;
}

/// One collapse round (Pass 1 export-alias drop → Pass 2 importer repoint → Pass 3
/// scope-aware consumer body rename). Returns the number of export aliases
/// relocated this round (0 means a fixpoint was reached).
fn collapse_round(
    files: &mut [EmittedFile],
    namespace_consumed: &BTreeSet<String>,
    island_paths: &BTreeMap<String, String>,
) -> usize {
    // Pass 1: drop each `Local as wire` export to `Local`, recording the relocation.
    // Runs on EVERY file now that islands are flattened to the source root (no
    // `modules/island/` prefix to gate on). This is safe: rewrite_cluster_exports
    // only collapses aliases whose EXPORTED name is a minified wire token (not a
    // readable identifier), so an intentional real-module `export { foo as bar }`
    // (bar readable) is preserved, while `export { foo as $FA }` collapses.
    let mut relocations = BTreeMap::<(String, String), String>::new();
    for file in files.iter_mut() {
        if namespace_consumed.contains(&file.path) {
            continue;
        }
        if !file.source.contains("export {") {
            continue;
        }
        file.source = rewrite_cluster_exports(&file.source, &file.path, &mut relocations);
    }
    if relocations.is_empty() {
        return 0;
    }

    // Pass 2: repoint every importer of a relocated wire at the exporter's new
    // (semantic) export name, preserving the importer's local binding.
    for file in files.iter_mut() {
        if !file.source.contains("import {") {
            continue;
        }
        file.source = rewrite_importers(&file.source, &file.path, &relocations, island_paths);
    }

    // Pass 3: rename consumer LOCAL bindings from the wire name to the real
    // (imported) name, scope-awarely, so consumer bodies read the semantic name
    // and the import alias collapses (`import { Real as wire }` → `import { Real }`).
    // Provably safe: only the local binding + its body references change; the
    // IMPORTED name is unchanged, so the cross-file export contract cannot break
    // (a colliding rename is skipped by the emitter, leaving a harmless alias).
    // This re-emit ALSO recovers the hub's own bare-wire exports to `Real as wire`
    // (the late readability hint), which the next round's Pass 1 then collapses.
    for file in files.iter_mut() {
        let renames = consumer_local_renames(&file.path, &file.source, island_paths);
        if renames.is_empty() {
            continue;
        }
        if let Some(rewritten) = rename_locals_scope_aware(&file.path, &file.source, &renames) {
            file.source = rewritten;
        }
    }
    relocations.len()
}

/// `(wire_local, real_imported)` pairs for this file's `import { Real as wire }`
/// specifiers from island clusters, where `Real` is a readable identifier and the
/// local is the minified wire name (so renaming the local to `Real` is a win).
fn consumer_local_renames(
    from_path: &str,
    source: &str,
    island_paths: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        let Some((names, spec_part)) = trimmed
            .strip_prefix("import { ")
            .and_then(|rest| rest.split_once(" } from "))
        else {
            continue;
        };
        let Some(spec) = unquote_trailing(spec_part) else {
            continue;
        };
        if resolve_island_target(from_path, &spec, island_paths).is_none() {
            continue;
        }
        for raw in names.split(',') {
            let item = raw.trim();
            let Some((imported, local)) = item.split_once(" as ") else {
                continue;
            };
            let (imported, local) = (imported.trim(), local.trim());
            if imported != local && is_readable_name(imported) {
                out.push((local.to_string(), imported.to_string()));
            }
        }
    }
    out
}

/// A recovered (non-minified) export name: length >= 4 with a 3+ lowercase run —
/// distinguishes `openClaudeSupportArticle` from wire names like `$7A` / `qPe`.
fn is_readable_name(name: &str) -> bool {
    if name.len() < 4 {
        return false;
    }
    let mut run = 0;
    for c in name.chars() {
        if c.is_ascii_lowercase() {
            run += 1;
            if run >= 3 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// Re-run the emitter's scope-aware renamer over `source`, renaming each
/// `wire -> real` local. Returns None if the file no longer parses (left as-is).
fn rename_locals_scope_aware(
    path: &str,
    source: &str,
    renames: &[(String, String)],
) -> Option<String> {
    let generated: Vec<reverts_js::GeneratedRename> = renames
        .iter()
        .map(|(wire, real)| reverts_js::GeneratedRename::new_all_scopes(wire.clone(), real.clone()))
        .collect();
    let p = std::path::Path::new(path);
    reverts_js::format_source_with_module_items_request(reverts_js::FormatSourceRequest {
        body_source: source,
        generated_imports: &[],
        generated_exports: &[],
        readability_renames: &generated,
        function_param_renames: &[],
        type_annotations: &[],
        infer_literal_types: false,
        path_hint: Some(p),
        importer_path: Some(p),
        goal: reverts_js::ParseGoal::TypeScript,
        lowering: reverts_js::CompilerLowering::None,
    })
    .ok()
}

/// Rewrite an exporter's `export { … };` clauses, dropping ` as wire` for each
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
                // Collapse only when the exported name is a minified WIRE token
                // (not a readable identifier): drop `foo as $FA` → `foo`, but keep
                // an intentional `foo as bar` (bar readable) so a real module's
                // deliberate public API name survives. Also requires the local to
                // be unique in the clause so the drop can't duplicate a name.
                Some(wire)
                    if !is_readable_name(wire)
                        && export_name_count.get(local).copied().unwrap_or(0) == 1 =>
                {
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

/// Extension-stripped path -> emitted path, for EVERY file, so a relative import
/// specifier can be resolved back to its exporting file. (Islands are now flat at
/// the source root, so there is no exporter-prefix to filter on; any file may be
/// an exporter whose wire aliases collapse.)
fn island_path_index(files: &[EmittedFile]) -> BTreeMap<String, String> {
    files
        .iter()
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
    fn entrypoint_hub_alias_collapses_and_consumer_renames() {
        // The hub re-exports a binding (bound from an owner at its top) under a
        // wire name; a consumer imports it from the hub by wire name.
        let mut files = vec![
            f(
                "modules/entrypoint.ts",
                "import { eventSourceOnEventField } from './island/net/sse.js';\nexport { eventSourceOnEventField as $FA };",
            ),
            f(
                "modules/island/feature/x.ts",
                "import { $FA } from '../../entrypoint.js';\nfunction h() { return $FA(); }\nexport { h };",
            ),
        ];
        collapse_island_wire_aliases(&mut files);
        // Hub export collapses to the real name.
        assert!(
            files[0]
                .source
                .contains("export { eventSourceOnEventField };"),
            "hub export collapsed: {}",
            files[0].source
        );
        // Consumer body now reads the real name; the wire name is gone.
        assert!(
            files[1].source.contains("eventSourceOnEventField()")
                && !files[1].source.contains("$FA"),
            "consumer body renamed, wire name gone: {}",
            files[1].source
        );
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
    fn consumed_alias_relocates_and_consumer_body_renames() {
        let mut files = vec![
            f(
                "modules/island/auth/oauth.ts",
                "export { InvalidRequestError as TFA };",
            ),
            f(
                "modules/island/agent/x.ts",
                "import { TFA } from '../auth/oauth.js';\nfunction g() { return new TFA(); }\nexport { g };",
            ),
        ];
        collapse_island_wire_aliases(&mut files);
        // Export collapses to the semantic name.
        assert_eq!(files[0].source, "export { InvalidRequestError };");
        // Consumer import collapses to bare + the BODY reads the semantic name
        // (Pass 3 scope-aware rename); the wire name `TFA` is gone entirely.
        assert!(
            files[1].source.contains("import { InvalidRequestError }"),
            "import collapsed: {}",
            files[1].source
        );
        assert!(
            files[1].source.contains("new InvalidRequestError()")
                && !files[1].source.contains("TFA"),
            "body renamed, no wire name remains: {}",
            files[1].source
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
    fn preserves_intentional_readable_alias_anywhere() {
        // After flattening, the pass runs on ALL files (no `modules/island/` gate).
        // A real-module export under a READABLE alias is a deliberate public name
        // and must be preserved; only minified wire aliases collapse.
        let mut files = vec![f(
            "auth/oauth-constants.ts",
            "export { internalToken as publicToken };",
        )];
        collapse_island_wire_aliases(&mut files);
        assert_eq!(files[0].source, "export { internalToken as publicToken };");
    }

    #[test]
    fn collapses_wire_alias_regardless_of_path() {
        // A wire alias on a now-flattened (non-`modules/island/`) path still collapses.
        let mut files = vec![f("auth/oauth.ts", "export { resolveToken as qPe };")];
        collapse_island_wire_aliases(&mut files);
        assert_eq!(files[0].source, "export { resolveToken };");
    }
}
