//! Pure string formatters for the planner's emitted body lines.
//!
//! Every helper here takes already-validated structural data (binding
//! names, specifiers, sets of bindings) and returns the matching JS
//! source line. The formatters intentionally produce a single canonical
//! shape per concept so the `parse_generated_*` reverse-parsers (in
//! lib.rs) can recognise them later during the planner's coalescing
//! passes — keep formatter output and parser expectations in sync.
//!
//! The runtime-helper bodies (`lazy_module_helper_source`,
//! `lazy_value_helper_source`) are static text intentionally — the
//! planner reuses them verbatim wherever lazy thunks are needed, and the
//! emitter audits the resulting source through OXC like every other
//! emitted file (ADR 0001 still holds; these are inputs to OXC codegen,
//! not regex rewrites of someone else's source).

use std::collections::BTreeSet;

use reverts_graph::RuntimeNamespaceExport;
use reverts_ir::BindingName;
use reverts_js::{
    is_ascii_identifier_continue as is_identifier_continue,
    is_ascii_identifier_start as is_identifier_start, is_js_keyword,
};

pub(crate) fn named_import_statement<'a>(
    bindings: impl Iterator<Item = &'a BindingName>,
    specifier: &str,
) -> String {
    let names = bindings
        .map(BindingName::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    format!("import {{ {names} }} from '{specifier}';")
}

pub(crate) fn named_import_alias_statement<'a>(
    specifiers: impl Iterator<Item = (&'a str, &'a BindingName)>,
    source: &str,
) -> String {
    let names = specifiers
        .map(|(imported, local)| {
            if imported == local.as_str() {
                local.as_str().to_string()
            } else {
                format!("{imported} as {local}", local = local.as_str())
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("import {{ {names} }} from '{source}';")
}

pub(crate) fn default_named_import_alias_statement<'a>(
    default_binding: &BindingName,
    specifiers: impl Iterator<Item = (&'a str, &'a BindingName)>,
    source: &str,
) -> String {
    let names = specifiers
        .map(|(imported, local)| {
            if imported == local.as_str() {
                local.as_str().to_string()
            } else {
                format!("{imported} as {local}", local = local.as_str())
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "import {}, {{ {names} }} from '{source}';",
        default_binding.as_str()
    )
}

pub(crate) fn default_import_statement(binding: &BindingName, source: &str) -> String {
    format!("import {} from '{source}';", binding.as_str())
}

pub(crate) fn namespace_import_statement(binding: &BindingName, source: &str) -> String {
    format!("import * as {} from '{source}';", binding.as_str())
}

pub(crate) fn named_export_statement<'a>(
    bindings: impl Iterator<Item = &'a BindingName>,
) -> String {
    let names = bindings
        .map(BindingName::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    format!("export {{ {names} }};")
}

pub(crate) fn named_reexport_statement<'a>(
    bindings: impl Iterator<Item = &'a BindingName>,
    specifier: &str,
) -> String {
    let names = bindings
        .map(BindingName::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    format!("export {{ {names} }} from '{specifier}';")
}

pub(crate) fn variable_declaration_statement<'a>(
    bindings: impl Iterator<Item = &'a BindingName>,
) -> String {
    let names = bindings
        .map(BindingName::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    format!("var {names};")
}

pub(crate) fn runtime_helpers_path(source_file_id: u32) -> String {
    format!("modules/runtime/source-{source_file_id}-helpers.ts")
}

pub(crate) fn runtime_lazy_helpers_path() -> &'static str {
    "modules/runtime/lazy.ts"
}

pub(crate) fn runtime_helper_setter_name(binding: &BindingName) -> String {
    format!("__reverts_set_{}", binding.as_str())
}

pub(crate) fn runtime_helper_setter_declaration(binding: &BindingName) -> String {
    let setter = runtime_helper_setter_name(binding);
    let binding = binding.as_str();
    // Keep this as a hoisted function declaration. Runtime helpers can
    // participate in ESM cycles with writer modules that call the setter
    // during module evaluation; a `const` arrow thunk would be in TDZ in
    // that shape. `return X = value` is still one formatted line smaller
    // than the previous two-statement body while preserving hoisting.
    format!("function {setter}(value) {{ return {binding} = value; }}")
}

pub(crate) fn runtime_helper_setter_declarations(bindings: &BTreeSet<BindingName>) -> String {
    bindings
        .iter()
        .map(runtime_helper_setter_declaration)
        .collect::<Vec<_>>()
        .join("")
}

pub(crate) fn runtime_helper_import_statement(
    bindings: &BTreeSet<BindingName>,
    setter_bindings: &BTreeSet<BindingName>,
    lazy_helper_names: &[&'static str],
    specifier: &str,
) -> String {
    let mut names = bindings
        .iter()
        .map(|binding| binding.as_str().to_string())
        .collect::<Vec<_>>();
    names.extend(setter_bindings.iter().map(runtime_helper_setter_name));
    names.extend(lazy_helper_names.iter().map(|name| (*name).to_string()));
    format!("import {{ {} }} from '{specifier}';", names.join(", "))
}

pub(crate) fn node_require_prelude_statement() -> String {
    "import { createRequire } from 'node:module';\nvar require = createRequire(import.meta.url);"
        .to_string()
}

/// ESM polyfills for the CommonJS globals a recovered esbuild bundle still uses
/// (`require`, `__filename`, `__dirname`). esbuild's node-ESM banner references
/// these, but they are undefined in a real ES module, so reconstruct them from
/// `import.meta.url`. Returns `None` when the source needs none of them.
pub(crate) fn node_cjs_environment_prelude(
    needs_require: bool,
    needs_filename: bool,
    needs_dirname: bool,
) -> Option<String> {
    // `__dirname` is derived from `__filename`.
    let needs_filename = needs_filename || needs_dirname;
    if !needs_require && !needs_filename {
        return None;
    }
    let mut lines = Vec::new();
    if needs_require {
        lines.push("import { createRequire } from 'node:module';".to_string());
    }
    if needs_filename {
        lines.push(
            "import { fileURLToPath as __reverts_fileURLToPath } from 'node:url';".to_string(),
        );
    }
    if needs_dirname {
        lines.push("import { dirname as __reverts_dirname } from 'node:path';".to_string());
    }
    if needs_require {
        lines.push("var require = createRequire(import.meta.url);".to_string());
    }
    if needs_filename {
        lines.push("var __filename = __reverts_fileURLToPath(import.meta.url);".to_string());
    }
    if needs_dirname {
        lines.push("var __dirname = __reverts_dirname(__filename);".to_string());
    }
    Some(lines.join("\n"))
}

pub(crate) fn noop_function_statement(binding: &BindingName) -> String {
    format!("function {}() {{}}", binding.as_str())
}

pub(crate) fn runtime_namespace_export_statement(
    namespace_export: &RuntimeNamespaceExport,
) -> String {
    let properties = namespace_export
        .exports
        .iter()
        .map(|(export_name, binding)| {
            format!(
                "{}: {{ enumerable: true, get: () => {} }}",
                property_key_source(export_name),
                binding.as_str()
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Object.defineProperties({}, {{ {} }});",
        namespace_export.namespace.as_str(),
        properties
    )
}

fn property_key_source(key: &str) -> String {
    if key
        .as_bytes()
        .first()
        .is_some_and(|byte| is_identifier_start(*byte))
        && key.as_bytes()[1..]
            .iter()
            .all(|byte| is_identifier_continue(*byte))
        && !is_js_keyword(key)
    {
        key.to_string()
    } else {
        format!("{key:?}")
    }
}

pub(crate) fn lazy_module_helper_source() -> &'static str {
    "function lazyModule(factory) {\n  \
        let _$cached;\n  \
        return () => {\n    \
            if (_$cached) return _$cached.exports;\n    \
            var _$module = _$cached = { exports: {} };\n    \
            factory(_$module.exports, _$module);\n    \
            return _$module.exports;\n  \
        };\n\
    }"
}

pub(crate) fn lazy_value_helper_source() -> &'static str {
    "function lazyValue(factory) {\n  \
        let _$init = false;\n  \
        let _$val;\n  \
        return () => {\n    \
            if (!_$init) {\n      \
                _$init = true;\n      \
                _$val = factory();\n    \
            }\n    \
            return _$val;\n  \
        };\n\
    }"
}
