//! Post-emit: give a canonical name to module-namespace bindings that are bound
//! to a `require()`/`__toESM(require())` of a well-known module.
//!
//! esbuild hoists `import * as fs from 'node:fs'` into a minified module-scope
//! `const zA = require('node:fs')`, and that binding is then used as `zA.…`
//! everywhere (hundreds–thousands of references). The deterministic readability
//! hints can't recover its name (there is no usage/property/export evidence —
//! it's a namespace object), but the SPECIFIER tells us exactly what it is. This
//! pass renames such a minified local to a canonical name derived from the
//! specifier (`node:fs` → `nodeFs`, `node:fs/promises` → `nodeFsPromises`,
//! `crypto` → `nodeCrypto`), scope-awarely via the emitter's renamer. When the
//! binding is exported, the rename surfaces as `export { nodeFs as zA }`, which
//! the subsequent island wire-collapse propagates to every consumer.
//!
//! Runs BEFORE island_wire_collapse so that propagation happens. Only minified
//! locals of recognized modules are touched; an already-readable local or an
//! unknown specifier is left alone, and the emitter skips any colliding rename.

use reverts_emitter::EmittedFile;

pub(crate) fn canonicalize_require_namespaces(files: &mut [EmittedFile]) {
    for file in files.iter_mut() {
        if !file.source.contains("require(") {
            continue;
        }
        let renames = require_namespace_renames(&file.source);
        if renames.is_empty() {
            continue;
        }
        if let Some(rewritten) = rename_locals_scope_aware(&file.path, &file.source, &renames) {
            file.source = rewritten;
        }
    }
}

/// `(minified_local, canonical_name)` for each `const X = require('<known>')` line.
/// The chosen name avoids collisions: if the canonical name already occurs in the
/// file (e.g. a second `node:path` namespace when `nodePath` is already bound) or
/// was already picked, a numeric suffix is appended so the rename still applies.
fn require_namespace_renames(source: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut taken = std::collections::BTreeSet::<String>::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        let Some((local, spec)) = parse_require_binding(trimmed) else {
            continue;
        };
        if !is_minified(local) {
            continue;
        }
        let Some(canonical) = canonical_namespace_name(spec) else {
            continue;
        };
        if canonical == local {
            continue;
        }
        let chosen = available_name(&canonical, source, &taken);
        taken.insert(chosen.clone());
        out.push((local.to_string(), chosen));
    }
    out
}

/// `canonical` if it is not already a word in `source` and not already chosen;
/// otherwise `canonical2`, `canonical3`, … until free.
fn available_name(
    canonical: &str,
    source: &str,
    taken: &std::collections::BTreeSet<String>,
) -> String {
    let occurs = |name: &str| word_occurs(source, name) || taken.contains(name);
    if !occurs(canonical) {
        return canonical.to_string();
    }
    for n in 2.. {
        let candidate = format!("{canonical}{n}");
        if !occurs(&candidate) {
            return candidate;
        }
    }
    unreachable!("suffix search always terminates")
}

/// Whether `name` appears as a whole-identifier token in `source`.
fn word_occurs(source: &str, name: &str) -> bool {
    let mut from = 0;
    while let Some(off) = source[from..].find(name) {
        let start = from + off;
        let end = start + name.len();
        let before_ok = start == 0 || !is_ident_char(source.as_bytes()[start - 1] as char);
        let after_ok = end == source.len() || !is_ident_char(source.as_bytes()[end] as char);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Parse `const|let|var <ident> = [__toESM(]require('<spec>')…` → `(ident, spec)`.
fn parse_require_binding(line: &str) -> Option<(&str, &str)> {
    let rest = line
        .strip_prefix("const ")
        .or_else(|| line.strip_prefix("let "))
        .or_else(|| line.strip_prefix("var "))?;
    let (ident, after) = rest.split_once('=')?;
    let ident = ident.trim();
    if ident.is_empty() || !ident.chars().all(is_ident_char) {
        return None;
    }
    let after = after.trim();
    let after = after.strip_prefix("__toESM(").unwrap_or(after);
    let after = after.strip_prefix("require(")?;
    let bytes = after.as_bytes();
    let quote = *bytes.first()?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    let spec_rest = &after[1..];
    let end = spec_rest.find(quote as char)?;
    Some((ident, &spec_rest[..end]))
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

/// A minified namespace local: short, or starting with `$`, and not already a
/// readable word.
fn is_minified(name: &str) -> bool {
    if name.starts_with('$') {
        return true;
    }
    if name.len() > 4 {
        return false;
    }
    // 4-char names with a 3+ lowercase run read as words (e.g. "path"); skip them.
    let mut run = 0;
    for c in name.chars() {
        if c.is_ascii_lowercase() {
            run += 1;
            if run >= 3 {
                return false;
            }
        } else {
            run = 0;
        }
    }
    true
}

/// Canonical name for a known module specifier, or None for unrecognized ones.
/// Node builtins get a `node`-prefixed camelCase name; bare specifiers that name a
/// Node builtin are treated the same.
fn canonical_namespace_name(spec: &str) -> Option<String> {
    const NODE_BUILTINS: &[&str] = &[
        "assert",
        "async_hooks",
        "buffer",
        "child_process",
        "cluster",
        "console",
        "constants",
        "crypto",
        "dgram",
        "diagnostics_channel",
        "dns",
        "domain",
        "events",
        "fs",
        "fs/promises",
        "http",
        "http2",
        "https",
        "inspector",
        "module",
        "net",
        "os",
        "path",
        "path/posix",
        "path/win32",
        "perf_hooks",
        "process",
        "punycode",
        "querystring",
        "readline",
        "repl",
        "stream",
        "stream/promises",
        "string_decoder",
        "timers",
        "timers/promises",
        "tls",
        "trace_events",
        "tty",
        "url",
        "util",
        "util/types",
        "v8",
        "vm",
        "wasi",
        "worker_threads",
        "zlib",
    ];
    let base = spec.strip_prefix("node:").unwrap_or(spec);
    if !NODE_BUILTINS.contains(&base) {
        return None;
    }
    let camel = camelize(base);
    Some(format!("node{}", capitalize_first(&camel)))
}

/// `fs/promises` → `fsPromises`, `child_process` → `childProcess`.
fn camelize(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut upper_next = false;
    for c in value.chars() {
        if c == '/' || c == '_' {
            upper_next = true;
        } else if upper_next {
            out.extend(c.to_uppercase());
            upper_next = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn capitalize_first(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Re-run the emitter's scope-aware renamer over `source`, renaming each
/// `local -> canonical`. Returns None if the file no longer parses.
fn rename_locals_scope_aware(
    path: &str,
    source: &str,
    renames: &[(String, String)],
) -> Option<String> {
    let generated: Vec<reverts_js::GeneratedRename> = renames
        .iter()
        .map(|(local, canonical)| {
            reverts_js::GeneratedRename::new_all_scopes(local.clone(), canonical.clone())
        })
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
    fn canonical_names_for_node_builtins() {
        assert_eq!(
            canonical_namespace_name("node:path").as_deref(),
            Some("nodePath")
        );
        assert_eq!(canonical_namespace_name("fs").as_deref(), Some("nodeFs"));
        assert_eq!(
            canonical_namespace_name("node:fs/promises").as_deref(),
            Some("nodeFsPromises")
        );
        assert_eq!(
            canonical_namespace_name("child_process").as_deref(),
            Some("nodeChildProcess")
        );
        assert_eq!(canonical_namespace_name("./local").as_deref(), None);
        assert_eq!(canonical_namespace_name("lodash").as_deref(), None);
    }

    #[test]
    fn renames_minified_require_namespace_and_its_uses() {
        let mut files = vec![f(
            "runtime/_helpers/h.ts",
            "const AA = require('node:path');\nfunction g(p) { return AA.resolve(p); }\nexport { AA, g };",
        )];
        canonicalize_require_namespaces(&mut files);
        let s = &files[0].source;
        assert!(s.contains("const nodePath = require('node:path')"), "{s}");
        assert!(s.contains("nodePath.resolve(p)"), "{s}");
        assert!(!s.contains("AA.resolve"), "{s}");
    }

    #[test]
    fn leaves_readable_or_unknown_bindings_alone() {
        let mut files = vec![f(
            "x.ts",
            "const path = require('node:path');\nconst dep = require('./local.js');\nexport { path, dep };",
        )];
        canonicalize_require_namespaces(&mut files);
        let s = &files[0].source;
        assert!(
            s.contains("const path = require('node:path')"),
            "readable kept: {s}"
        );
        assert!(
            s.contains("const dep = require('./local.js')"),
            "unknown kept: {s}"
        );
    }
}
