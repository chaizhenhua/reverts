//! Discover esbuild wrapper-helper aliases inside a bundle.
//!
//! esbuild emits a tiny set of helper functions for lazy CJS/ESM module
//! initialisation. In dev (un-minified) output they retain their canonical
//! names `__commonJS` and `__esm`. In production bundles they are renamed
//! to opaque single-letter aliases (`U`, `O`, …) and *the call shape
//! changes*: instead of `__commonJS({"path": fn, ...})` registering many
//! modules at once, the minified form is one `var <name> = <alias>(fn)`
//! per module — the path key is lost entirely.
//!
//! This module recognises the helper definitions by AST shape, returning
//! the set of local aliases that bind to a CJS helper and an ESM helper
//! respectively. Both detectors in `esbuild.rs` use this lookup so they
//! work on both un-minified and minified bundles.

use oxc_ast::Visit;
use oxc_ast::ast::{
    Argument, AssignmentTarget, BindingPatternKind, CallExpression, Expression, ObjectPropertyKind,
    Program, PropertyKey, Statement,
};

/// Local names that the bundle has assigned to esbuild helpers.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EsbuildHelperAliases {
    pub commonjs: Vec<String>,
    pub esm: Vec<String>,
}

/// Walk top-level variable declarations and identify ones whose
/// initializer matches the esbuild `__commonJS` or `__esm` helper shape.
/// Returns the local names. The canonical `__commonJS` / `__esm` names
/// are NOT included here — call sites add them as static fallbacks.
#[must_use]
pub fn discover_aliases(program: &Program<'_>) -> EsbuildHelperAliases {
    let mut out = EsbuildHelperAliases::default();
    for stmt in &program.body {
        let Statement::VariableDeclaration(vd) = stmt else {
            continue;
        };
        for decl in &vd.declarations {
            let BindingPatternKind::BindingIdentifier(id) = &decl.id.kind else {
                continue;
            };
            let Some(init) = &decl.init else { continue };
            let Expression::ArrowFunctionExpression(outer) = init else {
                continue;
            };
            // Helper shape: (A, Q) => () => (<body>)
            // Outer: 2 params, expression body, returns an arrow.
            if outer.params.items.len() != 2 || outer.r#async {
                continue;
            }
            let outer_param_names: Vec<&str> = outer
                .params
                .items
                .iter()
                .filter_map(|p| match &p.pattern.kind {
                    BindingPatternKind::BindingIdentifier(b) => Some(b.name.as_str()),
                    _ => None,
                })
                .collect();
            if outer_param_names.len() != 2 {
                continue;
            }
            let Some(inner_expr) = single_expression_in_body(&outer.body.statements) else {
                continue;
            };
            let Expression::ArrowFunctionExpression(inner) = inner_expr else {
                continue;
            };
            // Inner: 0 params, expression body.
            if !inner.params.items.is_empty() || inner.r#async {
                continue;
            }
            let mut probe = ShapeProbe {
                outer_params: &outer_param_names,
                has_param_dot_exports: false,
                has_exports_obj_literal: false,
                has_param_self_assign_call: false,
            };
            probe.visit_function_body(&inner.body);
            if probe.has_param_dot_exports && probe.has_exports_obj_literal {
                out.commonjs.push(id.name.to_string());
            } else if probe.has_param_self_assign_call && !probe.has_exports_obj_literal {
                out.esm.push(id.name.to_string());
            }
        }
    }
    out
}

/// If the FunctionBody contains exactly one ExpressionStatement, return
/// its expression. Otherwise None.
fn single_expression_in_body<'a, 'b>(
    statements: &'b [Statement<'a>],
) -> Option<&'b Expression<'a>> {
    if statements.len() != 1 {
        return None;
    }
    match &statements[0] {
        Statement::ExpressionStatement(es) => Some(&es.expression),
        _ => None,
    }
}

struct ShapeProbe<'p> {
    outer_params: &'p [&'p str],
    has_param_dot_exports: bool,
    has_exports_obj_literal: bool,
    has_param_self_assign_call: bool,
}

impl<'a> Visit<'a> for ShapeProbe<'_> {
    fn visit_static_member_expression(
        &mut self,
        member: &oxc_ast::ast::StaticMemberExpression<'a>,
    ) {
        if member.property.name == "exports"
            && let Expression::Identifier(obj) = &member.object
            && self.outer_params.contains(&obj.name.as_str())
        {
            self.has_param_dot_exports = true;
        }
        oxc_ast::visit::walk::walk_static_member_expression(self, member);
    }

    fn visit_object_expression(&mut self, obj: &oxc_ast::ast::ObjectExpression<'a>) {
        for prop in &obj.properties {
            if let ObjectPropertyKind::ObjectProperty(p) = prop
                && let PropertyKey::StaticIdentifier(id) = &p.key
                && id.name == "exports"
            {
                self.has_exports_obj_literal = true;
            }
        }
        oxc_ast::visit::walk::walk_object_expression(self, obj);
    }

    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        // Detect `A(A=0)` shape: callee is a known outer param AND one of
        // the arguments is `<param> = 0` (assignment of literal 0 back to
        // the callee identifier).
        if let Expression::Identifier(callee) = &call.callee
            && self.outer_params.contains(&callee.name.as_str())
        {
            for arg in &call.arguments {
                let Argument::AssignmentExpression(ae) = arg else {
                    continue;
                };
                let AssignmentTarget::AssignmentTargetIdentifier(target) = &ae.left else {
                    continue;
                };
                if target.name != callee.name {
                    continue;
                }
                if matches!(&ae.right, Expression::NumericLiteral(n) if n.value == 0.0) {
                    self.has_param_self_assign_call = true;
                }
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

    fn discover(src: &str) -> EsbuildHelperAliases {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, src, SourceType::default()).parse();
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        discover_aliases(&parsed.program)
    }

    #[test]
    fn finds_minified_commonjs_helper_alias() {
        // Canonical esbuild __commonJS helper, renamed to U.
        let src = r#"var U=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports);"#;
        let aliases = discover(src);
        assert_eq!(aliases.commonjs, vec!["U".to_string()]);
        assert!(aliases.esm.is_empty());
    }

    #[test]
    fn finds_minified_esm_helper_alias() {
        // Canonical esbuild __esm helper, renamed to O.
        let src = r#"var O=(A,Q)=>()=>(A&&(Q=A(A=0)),Q);"#;
        let aliases = discover(src);
        assert_eq!(aliases.esm, vec!["O".to_string()]);
        assert!(aliases.commonjs.is_empty());
    }

    #[test]
    fn finds_both_helpers_in_one_program() {
        let src = r#"
            var U=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports);
            var O=(A,Q)=>()=>(A&&(Q=A(A=0)),Q);
        "#;
        let aliases = discover(src);
        assert_eq!(aliases.commonjs, vec!["U".to_string()]);
        assert_eq!(aliases.esm, vec!["O".to_string()]);
    }

    #[test]
    fn ignores_unrelated_two_param_arrow() {
        let src = r#"var add=(a,b)=>a+b;"#;
        let aliases = discover(src);
        assert!(aliases.commonjs.is_empty());
        assert!(aliases.esm.is_empty());
    }

    #[test]
    fn ignores_arrow_that_does_not_return_a_zero_arg_arrow() {
        let src = r#"var f=(a,b)=>a.exports;"#;
        let aliases = discover(src);
        assert!(aliases.commonjs.is_empty());
        assert!(aliases.esm.is_empty());
    }

    #[test]
    fn ignores_arrow_returning_arrow_but_wrong_inner_body() {
        // 2-param arrow returning a 0-param arrow but body has neither
        // `.exports` nor a self-assign call.
        let src = r#"var f=(a,b)=>()=>(a+b);"#;
        let aliases = discover(src);
        assert!(aliases.commonjs.is_empty());
        assert!(aliases.esm.is_empty());
    }

    #[test]
    fn finds_canonical_unminified_names() {
        // The un-minified names are also valid helper shapes. The
        // detector returns them too — call sites already add the
        // canonical names as static fallbacks, so duplication is fine.
        let src = r#"
            var __commonJS=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports);
            var __esm=(A,Q)=>()=>(A&&(Q=A(A=0)),Q);
        "#;
        let aliases = discover(src);
        assert_eq!(aliases.commonjs, vec!["__commonJS".to_string()]);
        assert_eq!(aliases.esm, vec!["__esm".to_string()]);
    }
}
