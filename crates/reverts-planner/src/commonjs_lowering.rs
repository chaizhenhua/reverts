//! Lower inlined CommonJS module bodies to ESM.
//!
//! A path-named bundle (Electron main process) inlines whole CommonJS packages
//! verbatim — the recovered bodies keep `require('./x')` and
//! `module.exports = {…}`. The ESM emitter only shims `require` (via
//! `createRequire`), which is not enough: `require('./x')` resolves against the
//! emitted file's location (where the rerouted sibling is not), `module.exports`
//! references an undefined `module` in an ES module, and — because reachability
//! and import-resolution only follow ESM `import` statements — a module reached
//! ONLY through `require()` is pruned and never emitted.
//!
//! Converting the CommonJS surface to ESM up front (before reachability/prune)
//! lets every downstream pass treat these as ordinary ES modules:
//!
//! - `const { a, b } = require('<rel>')` becomes `import { a, b } from '<emitted>'`
//! - `const ns = require('<rel>')` becomes `import * as ns from '<emitted>'`
//! - bare `require('<rel>');` becomes `import '<emitted>';`
//! - `module.exports = { … }` becomes per-member ESM exports: a shorthand or
//!   identifier-aliased member re-exports the existing binding (`export { name }`
//!   / `export { local as key }`), a value-expression member is declared and
//!   exported (`export const key = expr;`).
//!
//! Only RELATIVE requires that resolve to a known emitted module are rewritten
//! (bare `require('fs')` keeps its `createRequire` shim). Only an object-literal
//! `module.exports` with identifier keys is converted; any other shape is left
//! untouched rather than risk a wrong transform.

use std::collections::BTreeMap;

use reverts_model::EnrichedProgram;

use crate::EmitPlan;
use crate::byte_lexer::find_matching_brace;
use crate::module_output_path;
use crate::relative_paths::relative_import_specifier;

/// Returns the number of CommonJS modules rewritten.
pub(crate) fn lower_commonjs_modules_to_esm(
    program: &EnrichedProgram,
    plan: &mut EmitPlan,
) -> usize {
    // original module path (file path, e.g. `…/ws/lib/constants.js`) → emitted
    // output path, and the inverse keyed by emitted path.
    let mut emitted_by_original = BTreeMap::<String, String>::new();
    let mut original_by_emitted = BTreeMap::<String, String>::new();
    for module in program.model().modules() {
        let Some(emitted) = module_output_path(program, module.id) else {
            continue;
        };
        let original = module.original_name.clone();
        if original.contains('/') {
            emitted_by_original.insert(original.clone(), emitted.clone());
        }
        original_by_emitted.insert(emitted, original);
    }

    let mut rewritten = 0usize;
    for file in &mut plan.files {
        let Some(original) = original_by_emitted.get(&file.path) else {
            continue;
        };
        let body = file.body.join("\n");
        if !body.contains("module.exports") && !body.contains("require(") {
            continue;
        }
        let original_dir = parent_dir(original);
        let resolve = |specifier: &str| -> Option<String> {
            let target = resolve_relative_module(&original_dir, specifier, &emitted_by_original)?;
            Some(relative_import_specifier(&file.path, &target))
        };
        let lowered = lower_body(&body, &resolve);
        if lowered != body {
            file.body = vec![lowered];
            rewritten += 1;
        }
    }
    rewritten
}

fn lower_body(body: &str, resolve: &dyn Fn(&str) -> Option<String>) -> String {
    let with_imports = rewrite_relative_requires(body, resolve);
    rewrite_module_exports(&with_imports)
}

/// Convert relative `require('<rel>')` forms to ESM imports. Only requires whose
/// specifier resolves to a known emitted module are rewritten line-by-line; the
/// `require`/`module.exports` strings are single statements in the recovered CJS
/// source, so a line scan is sufficient and avoids disturbing the rest.
fn rewrite_relative_requires(body: &str, resolve: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = Vec::<String>::new();
    for line in body.lines() {
        out.push(rewrite_require_line(line, resolve).unwrap_or_else(|| line.to_string()));
    }
    out.join("\n")
}

fn rewrite_require_line(line: &str, resolve: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let trimmed = line.trim();
    let indent = &line[..line.len() - line.trim_start().len()];

    // `const { a, b } = require('<rel>');`  /  `let`/`var`
    for keyword in ["const ", "let ", "var "] {
        if let Some(rest) = trimmed.strip_prefix(keyword) {
            let rest = rest.trim();
            if let Some(inner) = rest.strip_prefix('{') {
                let (names, after) = inner.split_once('}')?;
                let after = after.trim_start();
                let spec = require_specifier(after.strip_prefix('=')?.trim())?;
                let target = resolve(&spec)?;
                let names = normalize_named_clause(names)?;
                return Some(format!("{indent}import {{ {names} }} from '{target}';"));
            }
            // `const ns = require('<rel>');`
            if let Some((binding, after)) = rest.split_once('=') {
                let binding = binding.trim();
                if is_identifier(binding) {
                    let spec = require_specifier(after.trim())?;
                    let target = resolve(&spec)?;
                    return Some(format!("{indent}import * as {binding} from '{target}';"));
                }
            }
        }
    }

    // bare `require('<rel>');` side-effect statement
    if trimmed.starts_with("require(") {
        let spec = require_specifier(trimmed)?;
        let target = resolve(&spec)?;
        return Some(format!("{indent}import '{target}';"));
    }
    None
}

/// Extract the string specifier from a `require('<spec>')` expression (the
/// expression must be exactly a require call, optionally `;`-terminated).
fn require_specifier(expr: &str) -> Option<String> {
    let expr = expr.trim().trim_end_matches(';').trim();
    let inner = expr.strip_prefix("require(")?.strip_suffix(')')?.trim();
    let specifier = inner
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .or_else(|| inner.strip_prefix('"').and_then(|s| s.strip_suffix('"')))?;
    (specifier.starts_with("./") || specifier.starts_with("../")).then(|| specifier.to_string())
}

/// Normalise a `{ a, b as c }` destructuring clause into an import clause. CJS
/// destructuring uses `:` for renames (`{ a: b }`), ESM imports use `as`.
fn normalize_named_clause(names: &str) -> Option<String> {
    let mut specifiers = Vec::new();
    for raw in names.split(',') {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let specifier = if let Some((imported, local)) = raw.split_once(':') {
            let (imported, local) = (imported.trim(), local.trim());
            if !is_identifier(imported) || !is_identifier(local) {
                return None;
            }
            if imported == local {
                imported.to_string()
            } else {
                format!("{imported} as {local}")
            }
        } else {
            if !is_identifier(raw) {
                return None;
            }
            raw.to_string()
        };
        specifiers.push(specifier);
    }
    (!specifiers.is_empty()).then(|| specifiers.join(", "))
}

/// Convert a single top-level `module.exports = { … };` object literal into ESM
/// exports. A member is mapped by shape so an EXISTING local binding is never
/// re-declared (which would be `Identifier 'X' has already been declared`):
/// - shorthand `name`        → `export { name }`           (existing binding)
/// - `key: localIdentifier`  → `export { localIdentifier as key }`
/// - `key: expression`       → `export const key = expression;`
fn rewrite_module_exports(body: &str) -> String {
    let Some(assign_pos) = find_top_level_module_exports_object(body) else {
        return body.to_string();
    };
    let brace_pos = body[assign_pos..]
        .find('{')
        .map(|offset| assign_pos + offset);
    let Some(brace_pos) = brace_pos else {
        return body.to_string();
    };
    let Some(close) = find_matching_brace(body, brace_pos) else {
        return body.to_string();
    };
    let object_literal = &body[brace_pos..=close];
    let Some((const_decls, named)) = object_literal_esm_exports(object_literal) else {
        return body.to_string();
    };
    if const_decls.is_empty() && named.is_empty() {
        return body.to_string();
    }
    // Span to replace: from `module.exports` to the `;` after the object.
    let after = close + 1;
    let stmt_end = body[after..]
        .find(';')
        .map_or(body.len(), |offset| after + offset + 1);
    let mut replacement = const_decls.join("\n");
    if !named.is_empty() {
        if !replacement.is_empty() {
            replacement.push('\n');
        }
        replacement.push_str(&format!("export {{ {} }};", named.join(", ")));
    }
    let mut out = String::with_capacity(body.len() + replacement.len());
    out.push_str(&body[..assign_pos]);
    out.push_str(&replacement);
    out.push_str(&body[stmt_end..]);
    out
}

/// Byte offset of a top-level `module.exports =` assignment whose RHS is an
/// object literal, or `None` if absent / not at statement start.
fn find_top_level_module_exports_object(body: &str) -> Option<usize> {
    let needle = "module.exports";
    let mut search = 0usize;
    while let Some(rel) = body[search..].find(needle) {
        let pos = search + rel;
        let at_stmt_start = body[..pos]
            .trim_end()
            .chars()
            .last()
            .is_none_or(|ch| matches!(ch, ';' | '{' | '}' | '\n'));
        let after = body[pos + needle.len()..].trim_start();
        if at_stmt_start && after.starts_with('=') && after[1..].trim_start().starts_with('{') {
            return Some(pos);
        }
        search = pos + needle.len();
    }
    None
}

/// Map a `{ … }` object literal's members to `(export-const declarations, named
/// export specifiers)`. Splits on top-level commas only. Returns `None` if any
/// member is not a plain identifier-keyed entry (spread, computed key, method) —
/// the caller then leaves `module.exports` untouched rather than risk a wrong
/// transform.
fn object_literal_esm_exports(object_literal: &str) -> Option<(Vec<String>, Vec<String>)> {
    let inner = object_literal.strip_prefix('{')?.strip_suffix('}')?;
    let mut const_decls = Vec::new();
    let mut named = Vec::new();
    for member in split_top_level_commas(inner) {
        let member = member.trim();
        if member.is_empty() {
            continue;
        }
        if member.starts_with("...") {
            return None;
        }
        match member.split_once(':') {
            None => {
                // Shorthand `name`: an existing local binding.
                if !is_identifier(member) {
                    return None;
                }
                named.push(member.to_string());
            }
            Some((key, value)) => {
                let key = key.trim();
                let value = value.trim();
                if !is_identifier(key) {
                    return None;
                }
                if is_identifier(value) {
                    // Alias of an existing binding: `export { value as key }`.
                    if value == key {
                        named.push(key.to_string());
                    } else {
                        named.push(format!("{value} as {key}"));
                    }
                } else {
                    // New value expression: declare + export it.
                    const_decls.push(format!("export const {key} = {value};"));
                }
            }
        }
    }
    Some((const_decls, named))
}

/// Split on commas that sit at bracket/brace/paren depth zero and outside
/// strings, so commas inside member values (`Symbol('x')`, arrays) don't split.
fn split_top_level_commas(source: &str) -> Vec<&str> {
    let bytes = source.as_bytes();
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut string: Option<u8> = None;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(quote) = string {
            if byte == b'\\' {
                index += 2;
                continue;
            }
            if byte == quote {
                string = None;
            }
            index += 1;
            continue;
        }
        match byte {
            b'\'' | b'"' | b'`' => string = Some(byte),
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                parts.push(&source[start..index]);
                start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    parts.push(&source[start..]);
    parts
}

fn resolve_relative_module(
    from_dir: &str,
    specifier: &str,
    emitted_by_original: &BTreeMap<String, String>,
) -> Option<String> {
    let resolved = normalize_join(from_dir, specifier);
    for candidate in [
        resolved.clone(),
        format!("{resolved}.js"),
        format!("{resolved}.cjs"),
        format!("{resolved}.mjs"),
        format!("{resolved}.json"),
        format!("{resolved}/index.js"),
    ] {
        if let Some(emitted) = emitted_by_original.get(&candidate) {
            return Some(emitted.clone());
        }
    }
    None
}

fn parent_dir(path: &str) -> String {
    path.rsplit_once('/')
        .map_or(String::new(), |(dir, _)| dir.to_string())
}

fn normalize_join(from_dir: &str, specifier: &str) -> String {
    let mut parts: Vec<&str> = if from_dir.is_empty() {
        Vec::new()
    } else {
        from_dir.split('/').collect()
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
    parts.join("/")
}

fn is_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.chars().enumerate().all(|(index, ch)| {
            ch == '_' || ch == '$' || ch.is_ascii_alphabetic() || (index > 0 && ch.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver(spec: &str) -> Option<String> {
        // Pretend `./constants` resolves to a rerouted sibling.
        (spec == "./constants").then(|| "./42-constants.js".to_string())
    }

    #[test]
    fn converts_destructured_require_to_named_import() {
        let body = "const { kForOnEventAttribute, kListener } = require('./constants');";
        let out = lower_body(body, &resolver);
        assert_eq!(
            out,
            "import { kForOnEventAttribute, kListener } from './42-constants.js';"
        );
    }

    #[test]
    fn converts_renamed_require_destructure_to_aliased_import() {
        let body = "const { a: b } = require('./constants');";
        let out = lower_body(body, &resolver);
        assert_eq!(out, "import { a as b } from './42-constants.js';");
    }

    #[test]
    fn leaves_bare_package_require_untouched() {
        let body = "const fs = require('fs');";
        assert_eq!(lower_body(body, &resolver), body);
    }

    #[test]
    fn converts_module_exports_object_to_esm_without_redeclaring_bindings() {
        // `a` is an existing binding (shorthand) → re-export, never re-declare.
        // `EMPTY`/`k` are new value expressions → declare + export.
        let body = "class a {}\nmodule.exports = { a, EMPTY: Buffer.alloc(0), k: Symbol('x') };\n";
        let out = lower_body(body, &resolver);
        assert!(
            out.contains("export const EMPTY = Buffer.alloc(0);"),
            "{out}"
        );
        assert!(out.contains("export const k = Symbol('x');"), "{out}");
        assert!(out.contains("export { a };"), "{out}");
        assert!(!out.contains("module.exports"), "{out}");
        assert!(
            !out.contains("export const { "),
            "must not re-declare existing bindings: {out}"
        );
    }

    #[test]
    fn converts_aliased_member_to_export_alias() {
        let body = "function impl() {}\nmodule.exports = { publicName: impl };\n";
        let out = lower_body(body, &resolver);
        assert!(out.contains("export { impl as publicName };"), "{out}");
    }

    #[test]
    fn leaves_non_object_module_exports_untouched() {
        let body = "module.exports = function foo() {};";
        assert_eq!(lower_body(body, &resolver), body);
    }

    #[test]
    fn split_top_level_commas_ignores_nested() {
        let parts = split_top_level_commas("a, b: Symbol('x,y'), c: [1, 2]");
        assert_eq!(parts.len(), 3);
    }
}
