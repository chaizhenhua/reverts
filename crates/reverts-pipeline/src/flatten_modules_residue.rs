//! Post-emit: flatten the residual `modules/` process-prefix files to semantic
//! source-root paths, recomputing every importer's relative specifier.
//!
//! Island clusters and node_modules sources are already placed at semantic paths
//! by the planner. What remains under `modules/` is synthetic infrastructure:
//! the entrypoint hub (`modules/entrypoint.ts`) and the bundler runtime helpers
//! (`modules/runtime/…`). Moving these in the planner would ripple through dozens
//! of unit-test fixtures that hardcode the old relative specifiers, so instead we
//! remap them here on the emitted text — the only consumer is the structural
//! audit, which runs right after and gates the result (a wrong specifier surfaces
//! as DanglingNamedImport / relative-import-target).
//!
//! Remap rules (semantic, no `modules/` prefix): `modules/entrypoint.ts` →
//! `entrypoint.ts` (the app entry hub); `modules/runtime/<rest>` →
//! `runtime/_helpers/<rest>` (synthetic helper, under a `_helpers/` subdir so it
//! never collides with a recovered `runtime/<name>.ts` module); any other
//! `modules/<rest>` → `<rest>` (drop the prefix).
//!
//! Every relative `import`/`export … from`/`import("…")` specifier is recomputed
//! from the new paths of both endpoints, so links stay correct regardless of which
//! side moved.

use std::collections::BTreeMap;

use reverts_emitter::EmittedFile;

use crate::EmittedAsset;

const MODULES_PREFIX: &str = "modules/";

/// Flatten residual `modules/` files to semantic paths in place, returning the
/// `old path -> new path` remap so callers can keep plan-derived metadata
/// (`module_output_paths`, `unmodularized_code_paths`, etc.) consistent with the
/// emitted file paths.
///
/// Emitted assets (`require('./addon.node')` targets etc.) live in the same
/// directory as their owning module file, and the `require(...)` specifiers that
/// point at them are NOT rewritten here (only `import`/`export` specifiers are).
/// So an asset must move in lockstep with its module: when `modules/x/m.ts`
/// flattens to `x/m.ts`, the sibling `modules/x/a.node` must become `x/a.node`
/// or the relative `./a.node` would dangle. We apply the same prefix-strip rule
/// to asset paths, sharing the `taken` collision set with the file remap.
pub(crate) fn flatten_modules_residue(
    files: &mut [EmittedFile],
    assets: &mut [EmittedAsset],
) -> BTreeMap<String, String> {
    // 1. Build the path remap (old emitted path -> new semantic path).
    let mut remap = BTreeMap::<String, String>::new();
    let existing: std::collections::BTreeSet<String> =
        files.iter().map(|f| f.path.clone()).collect();
    let mut taken = existing.clone();
    for file in files.iter() {
        if !file.path.starts_with(MODULES_PREFIX) {
            continue;
        }
        let new_path = remapped_path(&file.path, &mut taken);
        remap.insert(file.path.clone(), new_path);
    }
    // Move assets in lockstep with their module directory (records the mapping in
    // `remap` too, so callers can follow asset paths through the same table).
    for asset in assets.iter_mut() {
        if !asset.path.starts_with(MODULES_PREFIX) {
            continue;
        }
        let new_path = remapped_path(&asset.path, &mut taken);
        remap.insert(asset.path.clone(), new_path.clone());
        asset.path = new_path;
    }
    if remap.is_empty() {
        return remap;
    }

    // 2. Index every file's CURRENT extension-stripped path -> the resolved NEW
    //    extension-stripped path, for specifier recomputation.
    let new_stem_of: BTreeMap<String, String> = files
        .iter()
        .map(|f| {
            let new_path = remap
                .get(&f.path)
                .cloned()
                .unwrap_or_else(|| f.path.clone());
            (strip_ext(&f.path), strip_ext(&new_path))
        })
        .collect();

    // 3. Rewrite each file: recompute relative specifiers against the new paths,
    //    then move the file to its new path.
    for file in files.iter_mut() {
        let new_self = remap
            .get(&file.path)
            .cloned()
            .unwrap_or_else(|| file.path.clone());
        file.source = rewrite_specifiers(&file.path, &new_self, &file.source, &new_stem_of);
        file.path = new_self;
    }
    remap
}

/// The semantic destination for a `modules/…` path, de-duplicated against taken paths.
fn remapped_path(path: &str, taken: &mut std::collections::BTreeSet<String>) -> String {
    let base = if path == "modules/entrypoint.ts" {
        "entrypoint.ts".to_string()
    } else if let Some(rest) = path.strip_prefix("modules/runtime/") {
        format!("runtime/_helpers/{rest}")
    } else if let Some(rest) = path.strip_prefix(MODULES_PREFIX) {
        rest.to_string()
    } else {
        path.to_string()
    };
    if !taken.contains(&base) || base == path {
        taken.insert(base.clone());
        return base;
    }
    let (stem, ext) = base
        .rsplit_once('.')
        .map_or((base.as_str(), ""), |(s, e)| (s, e));
    for n in 2.. {
        let candidate = if ext.is_empty() {
            format!("{stem}-{n}")
        } else {
            format!("{stem}-{n}.{ext}")
        };
        if !taken.contains(&candidate) {
            taken.insert(candidate.clone());
            return candidate;
        }
    }
    unreachable!("suffix search always terminates")
}

/// Recompute each relative specifier in `source` for a file moving from
/// `old_self` to `new_self`, mapping the resolved target through `new_stem_of`.
fn rewrite_specifiers(
    old_self: &str,
    new_self: &str,
    source: &str,
    new_stem_of: &BTreeMap<String, String>,
) -> String {
    let old_dir = parent_dir(old_self);
    let new_dir = parent_dir(new_self);
    let mut out = String::with_capacity(source.len());
    for (i, line) in source.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&rewrite_line_specifiers(
            line,
            &old_dir,
            &new_dir,
            new_stem_of,
        ));
    }
    if source.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Rewrite every `'<rel>'` / `"<rel>"` module specifier on one line.
fn rewrite_line_specifiers(
    line: &str,
    old_dir: &str,
    new_dir: &str,
    new_stem_of: &BTreeMap<String, String>,
) -> String {
    // Only lines that carry a module specifier — import / export-from /
    // dynamic import. Quick reject keeps unrelated lines byte-identical.
    let trimmed = line.trim_start();
    let is_specifier_line = (trimmed.starts_with("import") || trimmed.starts_with("export"))
        && line.contains(" from ")
        || trimmed.starts_with("import ")
        || line.contains("import(");
    if !is_specifier_line && !line.contains("from '") && !line.contains("from \"") {
        return line.to_string();
    }
    let mut result = String::with_capacity(line.len());
    let bytes = line.as_bytes();
    let mut idx = 0;
    while idx < line.len() {
        let ch = bytes[idx] as char;
        if (ch == '\'' || ch == '"')
            && let Some(end_rel) = find_closing(line, idx + 1, ch)
        {
            let spec = &line[idx + 1..end_rel];
            if (spec.starts_with("./") || spec.starts_with("../"))
                && let Some(new_spec) = remap_specifier(spec, old_dir, new_dir, new_stem_of)
            {
                result.push(ch);
                result.push_str(&new_spec);
                result.push(ch);
            } else {
                // Keep the original quoted string verbatim.
                result.push_str(&line[idx..=end_rel]);
            }
            idx = end_rel + 1;
            continue;
        }
        result.push(ch);
        idx += 1;
    }
    result
}

fn find_closing(line: &str, from: usize, quote: char) -> Option<usize> {
    line[from..].find(quote).map(|offset| from + offset)
}

/// Map a relative specifier (resolved against `old_dir`) to the equivalent
/// specifier from `new_dir`, following any path remap of the target.
fn remap_specifier(
    spec: &str,
    old_dir: &str,
    new_dir: &str,
    new_stem_of: &BTreeMap<String, String>,
) -> Option<String> {
    let ext = source_ext(spec);
    let old_target = resolve(old_dir, strip_ext(spec).as_str());
    let new_target = new_stem_of.get(&old_target).cloned().unwrap_or(old_target);
    Some(relative_specifier(new_dir, &new_target, ext))
}

fn parent_dir(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => String::new(),
    }
}

fn resolve(base_dir: &str, rel_stem: &str) -> String {
    let mut segments: Vec<&str> = if base_dir.is_empty() {
        Vec::new()
    } else {
        base_dir.split('/').collect()
    };
    for part in rel_stem.split('/') {
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

/// Build a relative specifier from `from_dir` to `target` (extensionless),
/// re-appending `ext` (e.g. `.js`).
fn relative_specifier(from_dir: &str, target: &str, ext: &str) -> String {
    let from: Vec<&str> = if from_dir.is_empty() {
        Vec::new()
    } else {
        from_dir.split('/').collect()
    };
    let to: Vec<&str> = target.split('/').collect();
    let mut common = 0;
    while common < from.len() && common < to.len() && from[common] == to[common] {
        common += 1;
    }
    let ups = from.len() - common;
    let mut parts = Vec::new();
    for _ in 0..ups {
        parts.push("..".to_string());
    }
    for segment in &to[common..] {
        parts.push((*segment).to_string());
    }
    let joined = parts.join("/");
    let body = if ups == 0 {
        format!("./{joined}")
    } else {
        joined
    };
    format!("{body}{ext}")
}

fn source_ext(spec: &str) -> &str {
    for ext in [".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx"] {
        if spec.ends_with(ext) {
            return ext;
        }
    }
    ""
}

fn strip_ext(path: &str) -> String {
    for ext in [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"] {
        if let Some(stripped) = path.strip_suffix(ext) {
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
    fn flattens_entrypoint_and_recomputes_consumer_specifier() {
        let mut files = vec![
            f("modules/entrypoint.ts", "export { a };"),
            f(
                "auth/oauth.ts",
                "import { a } from '../modules/entrypoint.js';",
            ),
        ];
        flatten_modules_residue(&mut files, &mut []);
        assert_eq!(files[0].path, "entrypoint.ts");
        // auth/oauth.ts (depth 2) → entrypoint.ts (root) = ../entrypoint.js
        assert_eq!(
            files[1].source, "import { a } from '../entrypoint.js';",
            "{}",
            files[1].source
        );
    }

    #[test]
    fn flattens_runtime_helper_under_helpers_subdir() {
        let mut files = vec![
            f("modules/runtime/source-1-helpers.ts", "export { h };"),
            f(
                "modules/entrypoint.ts",
                "import { h } from './runtime/source-1-helpers.js';",
            ),
        ];
        flatten_modules_residue(&mut files, &mut []);
        assert_eq!(files[0].path, "runtime/_helpers/source-1-helpers.ts");
        assert_eq!(files[1].path, "entrypoint.ts");
        // entrypoint.ts (root) → runtime/_helpers/source-1-helpers.ts
        assert!(
            files[1]
                .source
                .contains("from './runtime/_helpers/source-1-helpers.js'"),
            "{}",
            files[1].source
        );
    }

    #[test]
    fn recomputes_self_relative_imports_when_file_moves() {
        // The moved entrypoint imports a sibling island; its own dir changed.
        let mut files = vec![
            f(
                // Hub at modules/ imports a root-level island: from `modules/` up
                // to the root and into `auth/` is `../auth/oauth.js`.
                "modules/entrypoint.ts",
                "import { x } from '../auth/oauth.js';",
            ),
            f("auth/oauth.ts", "export { x };"),
        ];
        flatten_modules_residue(&mut files, &mut []);
        assert_eq!(files[0].path, "entrypoint.ts");
        // After the hub moves to root, the same target is `./auth/oauth.js`.
        assert!(
            files[0].source.contains("from './auth/oauth.js'"),
            "{}",
            files[0].source
        );
    }

    #[test]
    fn leaves_non_modules_files_untouched() {
        let mut files = vec![f("auth/oauth.ts", "import { y } from '../git/ops.js';")];
        flatten_modules_residue(&mut files, &mut []);
        assert_eq!(files[0].path, "auth/oauth.ts");
        assert_eq!(files[0].source, "import { y } from '../git/ops.js';");
    }

    #[test]
    fn moves_assets_in_lockstep_with_their_module() {
        // The module file flattens; its sibling asset (referenced by a non-rewritten
        // `require('./addon.node')`) must move to the same new directory so the
        // relative specifier still resolves.
        let mut files = vec![f(
            "modules/1-src/index.ts",
            "const a = require('./addon.node'); export { a };",
        )];
        let mut assets = vec![EmittedAsset {
            path: "modules/1-src/addon.node".to_string(),
            bytes: b"native".to_vec(),
            executable: false,
        }];
        let remap = flatten_modules_residue(&mut files, &mut assets);
        assert_eq!(files[0].path, "1-src/index.ts");
        assert_eq!(assets[0].path, "1-src/addon.node");
        // The require specifier is intentionally untouched, so `./addon.node`
        // resolves to `1-src/addon.node` — exactly where the asset now lives.
        assert!(
            files[0].source.contains("require('./addon.node')"),
            "{}",
            files[0].source
        );
        assert_eq!(
            remap.get("modules/1-src/addon.node").map(String::as_str),
            Some("1-src/addon.node")
        );
    }
}
