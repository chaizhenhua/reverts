use oxc_ast::Visit;
use oxc_ast::ast::{
    Argument, CallExpression, Expression, ObjectPropertyKind, Program, PropertyKey,
};
use oxc_span::GetSpan;
use reverts_ir::{ByteRange, ModuleId};

use crate::inner_module::{BundlerKind, InnerModule};

/// Recognise esbuild's `__commonJS({"key": (exports, module) => { … }})`
/// registration map. Each map entry becomes one `InnerModule` whose
/// `body_span` covers the arrow function body so the parent program
/// can be sliced into independent compilable units.
#[must_use]
pub fn detect_commonjs(program: &Program<'_>, parent_module_id: ModuleId) -> Vec<InnerModule> {
    detect_by_callee_name(program, parent_module_id, "__commonJS")
}

/// Recognise esbuild's `__esm({"key": () => { … }})` registration map
/// for ESM modules. Behaves identically to [`detect_commonjs`] but
/// matches the `__esm` callee name.
#[must_use]
pub fn detect_esm(program: &Program<'_>, parent_module_id: ModuleId) -> Vec<InnerModule> {
    detect_by_callee_name(program, parent_module_id, "__esm")
}

/// Shared implementation: walk the program, find every CallExpression
/// whose callee is the named identifier and whose first argument is an
/// object literal of `"path": (...) => { ... }` registrations.
fn detect_by_callee_name(
    program: &Program<'_>,
    parent_module_id: ModuleId,
    callee_name: &'static str,
) -> Vec<InnerModule> {
    let mut collected = Vec::new();
    let mut visitor = NamedRegistryVisitor {
        out: &mut collected,
        parent_module_id,
        callee_name,
    };
    visitor.visit_program(program);
    collected
}

struct NamedRegistryVisitor<'a> {
    out: &'a mut Vec<InnerModule>,
    parent_module_id: ModuleId,
    callee_name: &'static str,
}

impl<'a> Visit<'a> for NamedRegistryVisitor<'_> {
    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if let Expression::Identifier(callee) = &call.callee
            && callee.name == self.callee_name
            && let Some(Argument::ObjectExpression(obj)) = call.arguments.first()
        {
            for prop in &obj.properties {
                let ObjectPropertyKind::ObjectProperty(p) = prop else {
                    continue;
                };
                let key_text = match &p.key {
                    PropertyKey::StaticIdentifier(id) => id.name.as_str().to_string(),
                    PropertyKey::StringLiteral(s) => s.value.as_str().to_string(),
                    _ => continue,
                };
                let body_span = match &p.value {
                    Expression::ArrowFunctionExpression(a) => {
                        let s = a.body.span();
                        ByteRange::new(s.start, s.end)
                    }
                    Expression::FunctionExpression(f) => {
                        let Some(body) = f.body.as_ref() else {
                            continue;
                        };
                        let s = body.span();
                        ByteRange::new(s.start, s.end)
                    }
                    _ => continue,
                };
                self.out.push(InnerModule {
                    virtual_id: format!("esbuild:{}", key_text),
                    body_span,
                    bundler: BundlerKind::Esbuild,
                    source_path_hint: Some(key_text),
                    parent_module_id: self.parent_module_id,
                });
            }
        }
        oxc_ast::visit::walk::walk_call_expression(self, call);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn extract(src: &str) -> Vec<InnerModule> {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, src, SourceType::default()).parse();
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        detect_commonjs(&parsed.program, ModuleId(99))
    }

    #[test]
    fn detect_commonjs_extracts_arrow_module_body() {
        let src = r#"
            var x = __commonJS({
                "node_modules/lodash/index.js": (exports, module) => {
                    module.exports = { map: function () {} };
                }
            });
        "#;
        let modules = extract(src);
        assert_eq!(modules.len(), 1);
        let m = &modules[0];
        assert_eq!(m.bundler, BundlerKind::Esbuild);
        assert_eq!(
            m.source_path_hint.as_deref(),
            Some("node_modules/lodash/index.js")
        );
        assert!(m.virtual_id.starts_with("esbuild:"));
        assert!(m.body_span.end > m.body_span.start);
        assert_eq!(m.parent_module_id, ModuleId(99));
    }

    #[test]
    fn detect_commonjs_extracts_multiple_entries() {
        let src = r#"
            var x = __commonJS({
                "a.js": (exports, module) => { module.exports = 1; },
                "b.js": (exports, module) => { module.exports = 2; }
            });
        "#;
        let modules = extract(src);
        assert_eq!(modules.len(), 2);
        let paths: Vec<_> = modules
            .iter()
            .filter_map(|m| m.source_path_hint.as_deref())
            .collect();
        assert!(paths.contains(&"a.js"));
        assert!(paths.contains(&"b.js"));
    }

    #[test]
    fn detect_commonjs_ignores_calls_with_wrong_callee() {
        let src = r#"
            var x = __notCommonJS({
                "a.js": (exports, module) => { module.exports = 1; }
            });
        "#;
        assert!(extract(src).is_empty());
    }

    #[test]
    fn detect_commonjs_ignores_calls_with_non_object_arg() {
        let src = r#"var x = __commonJS([]);"#;
        assert!(extract(src).is_empty());
    }

    #[test]
    fn detect_commonjs_returns_body_span_not_full_function_span() {
        let src = r#"var x = __commonJS({ "a": (e, m) => { var y = 1; m.exports = y; } });"#;
        let modules = extract(src);
        let m = &modules[0];
        let body_text = &src[m.body_span.start as usize..m.body_span.end as usize];
        assert!(body_text.starts_with('{'));
        assert!(body_text.ends_with('}'));
        assert!(body_text.contains("var y = 1"));
    }

    #[test]
    fn detect_esm_extracts_zero_arg_arrow_body() {
        let src = r#"
        var x = __esm({
            "lib/foo.js": () => {
                init_lib();
                foo = 1;
            }
        });
    "#;
        let modules = extract_esm(src);
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].bundler, BundlerKind::Esbuild);
        assert_eq!(modules[0].source_path_hint.as_deref(), Some("lib/foo.js"));
        assert!(modules[0].virtual_id.starts_with("esbuild:"));
    }

    #[test]
    fn detect_esm_ignores_non_esm_calls() {
        let src = r#"var x = __notEsm({ "a": () => {} });"#;
        assert!(extract_esm(src).is_empty());
    }

    fn extract_esm(src: &str) -> Vec<InnerModule> {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, src, SourceType::default()).parse();
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        detect_esm(&parsed.program, ModuleId(99))
    }
}
