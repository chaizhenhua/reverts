use oxc_ast::Visit;
use oxc_ast::ast::{
    Argument, BindingPatternKind, CallExpression, Expression, ObjectPropertyKind, Program,
    PropertyKey, Statement, VariableDeclarator,
};
use oxc_span::GetSpan;
use reverts_ir::{ByteRange, ModuleId};

use crate::detectors::esbuild_helpers::discover_aliases;
use crate::inner_module::{BundlerKind, InnerModule};

/// Recognise esbuild's `__commonJS` registration forms — both the
/// un-minified `__commonJS({"path": fn, ...})` map and the minified
/// `var <name> = <alias>(fn)` per-module call.
///
/// The detector first discovers local names bound to the `__commonJS` helper
/// by AST shape (see [`discover_aliases`]) then matches every call/assignment
/// that uses one of those proven helper names.
#[must_use]
pub fn detect_commonjs(program: &Program<'_>, parent_module_id: ModuleId) -> Vec<InnerModule> {
    let aliases = discover_aliases(program);
    let mut out = detect_named_registry(program, parent_module_id, &aliases.commonjs);
    out.extend(detect_var_assignment_modules(
        program,
        parent_module_id,
        &aliases.commonjs,
    ));
    out
}

/// Recognise esbuild's `__esm` registration forms — both the
/// un-minified `__esm({"path": fn, ...})` map and the minified
/// `var <name> = <alias>(fn)` per-module call.
#[must_use]
pub fn detect_esm(program: &Program<'_>, parent_module_id: ModuleId) -> Vec<InnerModule> {
    let aliases = discover_aliases(program);
    let mut out = detect_named_registry(program, parent_module_id, &aliases.esm);
    out.extend(detect_var_assignment_modules(
        program,
        parent_module_id,
        &aliases.esm,
    ));
    out
}

/// Un-minified form: `<callee>({"path1": fn, "path2": fn, …})`.
/// Each property of the object literal becomes one `InnerModule`.
fn detect_named_registry(
    program: &Program<'_>,
    parent_module_id: ModuleId,
    callee_names: &[String],
) -> Vec<InnerModule> {
    let mut out = Vec::new();
    let mut visitor = NamedRegistryVisitor {
        out: &mut out,
        parent_module_id,
        callee_names,
    };
    visitor.visit_program(program);
    out
}

/// Minified form: top-level `var <name> = <alias>(<arrow>)`. Each such
/// declaration becomes one `InnerModule` whose `body_span` covers the
/// arrow's body. The source path key is lost during minification, so
/// `source_path_hint` is `None` and `virtual_id` derives from the
/// binding name (`esbuild:<name>`).
fn detect_var_assignment_modules(
    program: &Program<'_>,
    parent_module_id: ModuleId,
    aliases: &[String],
) -> Vec<InnerModule> {
    if aliases.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for stmt in &program.body {
        let Statement::VariableDeclaration(vd) = stmt else {
            continue;
        };
        // esbuild scope-hoisting declares a module's top-level vars in the same
        // `var` statement as its init handle (`var a,b,X=helper(()=>{...})`),
        // with only initializer code in the arrow body. When the statement has
        // exactly one handle declarator we own the WHOLE statement, so the
        // hoisted sibling declarators and the handle become definitions rather
        // than free variables. With multiple handles in one statement there is
        // no single contiguous owning unit, so we fall back to per-handle
        // arrow-body spans (the hoisted vars stay unowned — a measured residual).
        let handle_count = vd
            .declarations
            .iter()
            .filter(|decl| is_helper_init_declarator(decl, aliases))
            .count();
        let statement_span = ByteRange::new(vd.span().start, vd.span().end);
        for decl in &vd.declarations {
            let BindingPatternKind::BindingIdentifier(binding) = &decl.id.kind else {
                continue;
            };
            let Some(Expression::CallExpression(call)) = decl.init.as_ref() else {
                continue;
            };
            let Expression::Identifier(callee_id) = &call.callee else {
                continue;
            };
            if !aliases.iter().any(|a| a == callee_id.name.as_str()) {
                continue;
            }
            let Some(arg) = call.arguments.first() else {
                continue;
            };
            let arrow_body_span = match arg {
                Argument::ArrowFunctionExpression(a) => {
                    let s = a.body.span();
                    ByteRange::new(s.start, s.end)
                }
                Argument::FunctionExpression(f) => {
                    let Some(body) = f.body.as_ref() else {
                        continue;
                    };
                    let s = body.span();
                    ByteRange::new(s.start, s.end)
                }
                _ => continue,
            };
            let body_span = if handle_count == 1 {
                statement_span
            } else {
                arrow_body_span
            };
            out.push(InnerModule {
                virtual_id: format!("esbuild:{}", binding.name.as_str()),
                body_span,
                bundler: BundlerKind::Esbuild,
                source_path_hint: None,
                parent_module_id,
            });
        }
    }
    out
}

/// A `var` declarator whose initializer is a call to a proven `__commonJS` /
/// `__esm` helper alias — i.e. an esbuild module init handle.
fn is_helper_init_declarator(decl: &VariableDeclarator<'_>, aliases: &[String]) -> bool {
    let BindingPatternKind::BindingIdentifier(_) = &decl.id.kind else {
        return false;
    };
    let Some(Expression::CallExpression(call)) = decl.init.as_ref() else {
        return false;
    };
    let Expression::Identifier(callee_id) = &call.callee else {
        return false;
    };
    aliases.iter().any(|a| a == callee_id.name.as_str())
}

struct NamedRegistryVisitor<'a, 'n> {
    out: &'a mut Vec<InnerModule>,
    parent_module_id: ModuleId,
    callee_names: &'n [String],
}

impl<'a> Visit<'a> for NamedRegistryVisitor<'_, '_> {
    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if let Expression::Identifier(callee) = &call.callee
            && self.callee_names.iter().any(|n| n == callee.name.as_str())
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

    #[test]
    fn detect_commonjs_extracts_arrow_module_body() {
        let src = r#"
            var __commonJS=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports);
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
            var __commonJS=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports);
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
        let src = r#"var __commonJS=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports); var x = __commonJS([]);"#;
        assert!(extract(src).is_empty());
    }

    #[test]
    fn detect_commonjs_returns_body_span_not_full_function_span() {
        let src = r#"var __commonJS=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports); var x = __commonJS({ "a": (e, m) => { var y = 1; m.exports = y; } });"#;
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
        var __esm=(A,Q)=>()=>(A&&(Q=A(A=0)),Q);
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

    #[test]
    fn var_assignment_module_owns_full_hoisted_statement() {
        // esbuild scope-hoisting puts a module's top-level vars + init handle
        // in the `var` statement, with only the initializer code in the arrow
        // body. The inner module must own the WHOLE statement so the hoisted
        // declarators (`a`, `b`) and the handle (`X`) become definitions,
        // not free variables.
        let src = r#"var st=(A,Q)=>()=>(A&&(Q=A(A=0)),Q);
var a,b,X=st(()=>{a=1;b=2});"#;
        let modules = extract_esm(src);
        assert_eq!(modules.len(), 1, "got: {modules:#?}");
        let m = &modules[0];
        assert_eq!(m.virtual_id, "esbuild:X");
        let owned = &src[m.body_span.start as usize..m.body_span.end as usize];
        assert!(
            owned.starts_with("var "),
            "owned span must be the full statement: {owned}"
        );
        assert!(
            owned.contains("a,b,X="),
            "must include hoisted declarators: {owned}"
        );
        assert!(
            owned.contains("a=1;b=2"),
            "must include the init body: {owned}"
        );
    }

    #[test]
    fn detect_commonjs_extracts_minified_var_assignment_modules() {
        // Production esbuild output: helper renamed to `U`, per-module
        // form is `var <name> = U((exports) => { ... })`.
        let src = r#"
            var U=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports);
            var vG=U((ES0)=>{ES0.foo=1});
            var Gc=U((CS0,m)=>{m.exports={bar:2}});
        "#;
        let modules = extract(src);
        assert_eq!(modules.len(), 2, "got: {modules:#?}");
        assert!(modules.iter().any(|m| m.virtual_id == "esbuild:vG"));
        assert!(modules.iter().any(|m| m.virtual_id == "esbuild:Gc"));
        // source_path_hint is lost in minification.
        for m in &modules {
            assert_eq!(m.source_path_hint, None);
            assert_eq!(m.bundler, BundlerKind::Esbuild);
        }
    }

    #[test]
    fn detect_esm_extracts_minified_var_assignment_modules() {
        // Production esbuild ESM: helper renamed to `O`, per-module form
        // is `var <name> = O(() => { ... })`.
        let src = r#"
            var O=(A,Q)=>()=>(A&&(Q=A(A=0)),Q);
            var $F1=O(()=>{Ez9=1});
            var Yj=O(()=>{$F1();ZK=2});
        "#;
        let modules = extract_esm(src);
        assert_eq!(modules.len(), 2, "got: {modules:#?}");
        assert!(modules.iter().any(|m| m.virtual_id == "esbuild:$F1"));
        assert!(modules.iter().any(|m| m.virtual_id == "esbuild:Yj"));
    }

    #[test]
    fn detect_commonjs_does_not_emit_for_var_assignment_without_helper() {
        // No helper definition → no aliases → no var-assignment extraction.
        let src = r#"
            var vG=U((ES0)=>{ES0.foo=1});
        "#;
        assert!(extract(src).is_empty());
    }

    #[test]
    fn detect_esm_does_not_confuse_cjs_alias_with_esm() {
        // Only CJS helper defined; ESM-form var-assignments using O should
        // not match (O has no alias).
        let src = r#"
            var U=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports);
            var x=O(()=>{a=1});
        "#;
        assert!(extract_esm(src).is_empty());
    }
}
