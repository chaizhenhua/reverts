//! Parse a CommonJS package `index.js` into its submodule → exported-name map.
//!
//! A barrel index re-exports each submodule under one or more public names. We
//! recover that mapping so a tree-shaken inlined package (whose in-bundle barrel
//! was dropped) can still be externalized: each recognized member unit's exports
//! object is rebound to the matching member(s) of the real package namespace.
//!
//! Two re-export shapes are recovered, both keyed by the submodule's relative
//! path (normalized, extension/`/index` stripped):
//! - WHOLE-object: `module.exports = { Range }` / `{ Range: require('./classes/range') }`
//!   — the package member `Range` IS the submodule's whole exports object
//!   (`member == None`).
//! - MEMBER-pick: `const re = require('./internal/re'); module.exports = { re: re.re,
//!   src: re.src, tokens: re.t }` — the package member `tokens` is the `.t`
//!   property of the submodule's exports object (`member == Some("t")`). The
//!   submodule's exports object is reconstructed from its member-picks.

use std::collections::BTreeMap;

use oxc_allocator::Allocator;
use oxc_ast::ast::{Expression, ModuleExportName, ObjectPropertyKind, Statement};
use oxc_parser::Parser;
use oxc_span::SourceType;

use crate::commonjs_exports::{commonjs_module_exports_target, static_property_key_name};

/// One way a submodule is re-exported by the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexReexport {
    /// Submodule relative path, normalized: `./classes/range.js` → `classes/range`.
    pub submodule_relpath: String,
    /// The public name the package exposes this under (`Range`, `tokens`).
    pub export_name: String,
    /// `Some(prop)` when the export is a property picked off the submodule's
    /// exports object (`re.t` → `Some("t")`); `None` when the export IS the whole
    /// submodule exports object.
    pub member: Option<String>,
}

/// A package index's full submodule → export-name re-export map.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageIndexReexports {
    pub reexports: Vec<IndexReexport>,
}

impl PackageIndexReexports {
    /// Every re-export of the given (normalized) submodule relative path.
    #[must_use]
    pub fn for_submodule(&self, relpath: &str) -> Vec<&IndexReexport> {
        self.reexports
            .iter()
            .filter(|reexport| reexport.submodule_relpath == relpath)
            .collect()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.reexports.is_empty()
    }
}

/// Normalize a `require` argument or anchor `export_specifier` to a comparable
/// submodule key: drop a leading `./`/`../`, a `<pkg>/` prefix is the caller's
/// job, and strip a trailing `.js`/`.cjs`/`.mjs`/`.json` and `/index`.
#[must_use]
pub fn normalize_submodule_relpath(path: &str) -> String {
    let mut value = path.trim();
    while let Some(rest) = value
        .strip_prefix("./")
        .or_else(|| value.strip_prefix("../"))
    {
        value = rest;
    }
    for ext in [".js", ".cjs", ".mjs", ".json"] {
        if let Some(stripped) = value.strip_suffix(ext) {
            value = stripped;
            break;
        }
    }
    if let Some(stripped) = value.strip_suffix("/index") {
        value = stripped;
    }
    value.to_string()
}

/// Parse a package `index.js` source into its re-export map. Returns an empty map
/// when the source does not parse or has no recognizable `module.exports = { … }`.
#[must_use]
pub fn parse_index_reexports(index_source: &str) -> PackageIndexReexports {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, index_source, SourceType::cjs()).parse();
    if parsed.panicked {
        return PackageIndexReexports::default();
    }
    let program = parsed.program;

    // local require binding name → normalized submodule relpath.
    let mut requires: BTreeMap<String, String> = BTreeMap::new();
    for statement in &program.body {
        let Statement::VariableDeclaration(declaration) = statement else {
            continue;
        };
        for declarator in &declaration.declarations {
            let (Some(name), Some(init)) = (declarator.id.get_identifier(), &declarator.init)
            else {
                continue;
            };
            if let Some(relpath) = require_call_relpath(init) {
                requires.insert(name.as_str().to_string(), relpath);
            }
        }
    }

    let mut reexports = Vec::new();
    for statement in &program.body {
        match statement {
            // CJS: `module.exports = { … }` / `exports = { … }`.
            Statement::ExpressionStatement(expression_statement) => {
                let Expression::AssignmentExpression(assignment) =
                    &expression_statement.expression
                else {
                    continue;
                };
                if !commonjs_module_exports_target(&assignment.left) {
                    continue;
                }
                let Expression::ObjectExpression(object) = &assignment.right else {
                    continue;
                };
                for property in &object.properties {
                    let ObjectPropertyKind::ObjectProperty(property) = property else {
                        continue;
                    };
                    let Some(export_name) = static_property_key_name(&property.key) else {
                        continue;
                    };
                    if let Some(reexport) =
                        resolve_property_value(&export_name, &property.value, &requires)
                    {
                        reexports.push(reexport);
                    }
                }
            }
            // ESM named re-export: `export { Y as X } from './x'`. The package
            // member `X` is the submodule's `Y` (member-pick on submodule `x`);
            // a bare `export { X } from './x'` is `Y == X`.
            Statement::ExportNamedDeclaration(export) => {
                let Some(source) = &export.source else {
                    continue; // local `export { … }` (no `from`) names no submodule
                };
                let raw = source.value.as_str();
                if !raw.starts_with('.') {
                    continue; // re-export from ANOTHER package — not a submodule
                }
                let relpath = normalize_submodule_relpath(raw);
                for specifier in &export.specifiers {
                    let (Some(local), Some(exported)) = (
                        module_export_name_text(&specifier.local),
                        module_export_name_text(&specifier.exported),
                    ) else {
                        continue;
                    };
                    reexports.push(IndexReexport {
                        submodule_relpath: relpath.clone(),
                        export_name: exported.to_string(),
                        member: Some(local.to_string()),
                    });
                }
            }
            _ => {}
        }
    }
    PackageIndexReexports { reexports }
}

/// The identifier text of a `ModuleExportName`, or `None` for a string-literal
/// export name (`export { x as "a-b" }` — not a usable submodule member).
fn module_export_name_text<'a>(name: &'a ModuleExportName<'a>) -> Option<&'a str> {
    match name {
        ModuleExportName::IdentifierName(identifier) => Some(identifier.name.as_str()),
        ModuleExportName::IdentifierReference(identifier) => Some(identifier.name.as_str()),
        ModuleExportName::StringLiteral(_) => None,
    }
}

/// The normalized relpath of a `require('./x')` call expression, else `None`.
fn require_call_relpath(expression: &Expression<'_>) -> Option<String> {
    let Expression::CallExpression(call) = expression else {
        return None;
    };
    let Expression::Identifier(callee) = &call.callee else {
        return None;
    };
    if callee.name != "require" || call.arguments.len() != 1 {
        return None;
    }
    let argument = call.arguments.first()?.as_expression()?;
    let Expression::StringLiteral(literal) = argument else {
        return None;
    };
    let raw = literal.value.as_str();
    if !raw.starts_with('.') {
        return None; // a bare specifier is another package, not a submodule
    }
    Some(normalize_submodule_relpath(raw))
}

/// Resolve one `module.exports` object property to an [`IndexReexport`], given the
/// known `require` bindings. Recognizes inline `require()`, a bare/shorthand
/// require-local, and a member access on a require-local.
fn resolve_property_value(
    export_name: &str,
    value: &Expression<'_>,
    requires: &BTreeMap<String, String>,
) -> Option<IndexReexport> {
    match value {
        // `Range: require('./classes/range')`
        Expression::CallExpression(_) => require_call_relpath(value).map(|relpath| IndexReexport {
            submodule_relpath: relpath,
            export_name: export_name.to_string(),
            member: None,
        }),
        // `Range` (shorthand) or `Range: SemVer` — a require-local.
        Expression::Identifier(identifier) => {
            requires
                .get(identifier.name.as_str())
                .map(|relpath| IndexReexport {
                    submodule_relpath: relpath.clone(),
                    export_name: export_name.to_string(),
                    member: None,
                })
        }
        // `tokens: internalRe.t` — a property of a require-local.
        Expression::StaticMemberExpression(member) => {
            let Expression::Identifier(object) = &member.object else {
                return None;
            };
            requires
                .get(object.name.as_str())
                .map(|relpath| IndexReexport {
                    submodule_relpath: relpath.clone(),
                    export_name: export_name.to_string(),
                    member: Some(member.property.name.as_str().to_string()),
                })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(map: &PackageIndexReexports, relpath: &str) -> Vec<(String, Option<String>)> {
        let mut out: Vec<(String, Option<String>)> = map
            .for_submodule(relpath)
            .into_iter()
            .map(|reexport| (reexport.export_name.clone(), reexport.member.clone()))
            .collect();
        out.sort();
        out
    }

    #[test]
    fn whole_object_inline_require() {
        let map = parse_index_reexports("module.exports = { Range: require('./classes/range') };");
        assert_eq!(
            names(&map, "classes/range"),
            vec![("Range".to_string(), None)]
        );
    }

    #[test]
    fn whole_object_prerequired_shorthand() {
        let map = parse_index_reexports(
            "const Range = require('./classes/range');\nmodule.exports = { Range };",
        );
        assert_eq!(
            names(&map, "classes/range"),
            vec![("Range".to_string(), None)]
        );
    }

    #[test]
    fn member_pick_multi_name() {
        // semver's shape: one required object, three picked members under renamed keys.
        let map = parse_index_reexports(
            "const internalRe = require('./internal/re');\n\
             module.exports = { re: internalRe.re, src: internalRe.src, tokens: internalRe.t };",
        );
        assert_eq!(
            names(&map, "internal/re"),
            vec![
                ("re".to_string(), Some("re".to_string())),
                ("src".to_string(), Some("src".to_string())),
                ("tokens".to_string(), Some("t".to_string())),
            ]
        );
    }

    #[test]
    fn esm_named_reexport_from_submodule() {
        // ESM barrel: `export { X } from './x'` and `export { Y as Z } from './y'`.
        let map = parse_index_reexports(
            "export { Range } from './classes/range.js';\n\
             export { compareBuild as compare } from './functions/compare.js';\n",
        );
        assert_eq!(
            names(&map, "classes/range"),
            vec![("Range".to_string(), Some("Range".to_string()))]
        );
        // `compareBuild as compare`: package exposes `compare`, picked off the
        // submodule's `compareBuild`.
        assert_eq!(
            names(&map, "functions/compare"),
            vec![("compare".to_string(), Some("compareBuild".to_string()))]
        );
    }

    #[test]
    fn esm_local_export_and_foreign_reexport_are_ignored() {
        // `export { X }` (no `from`) names no submodule; `export … from 'pkg'`
        // (bare specifier) is another package, not a submodule.
        let map = parse_index_reexports(
            "const X = 1;\nexport { X };\nexport { y } from '@scope/other';\n",
        );
        assert!(map.is_empty(), "{map:?}");
    }

    #[test]
    fn ignores_non_submodule_and_unparseable() {
        // A bare function export (not a require) and a foreign package are skipped.
        let map = parse_index_reexports(
            "const helper = () => 1;\nconst lodash = require('lodash');\n\
             module.exports = { helper, lodash };",
        );
        assert!(map.is_empty(), "{map:?}");
        assert!(parse_index_reexports("this is not js {{{").is_empty());
        assert!(parse_index_reexports("const x = 1;").is_empty());
    }

    #[test]
    fn normalizes_relpaths() {
        assert_eq!(
            normalize_submodule_relpath("./classes/range.js"),
            "classes/range"
        );
        assert_eq!(
            normalize_submodule_relpath("classes/range"),
            "classes/range"
        );
        assert_eq!(normalize_submodule_relpath("./a/b/index.js"), "a/b");
    }
}
