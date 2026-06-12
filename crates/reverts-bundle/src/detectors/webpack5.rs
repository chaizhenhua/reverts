use oxc_ast::ast::{
    Expression, ObjectPropertyKind, Program, PropertyKey, Statement, VariableDeclarator,
};
use oxc_span::GetSpan;
use reverts_ir::{ByteRange, ModuleId};

use crate::inner_module::{BundlerKind, InnerModule};

/// Recognise webpack 5's `var __webpack_modules__ = { … };` module map.
/// Each property's value is a factory function whose body is the module
/// implementation; we slice that body as the inner module.
#[must_use]
pub fn detect(program: &Program<'_>, parent_module_id: ModuleId) -> Vec<InnerModule> {
    let mut out = Vec::new();
    for stmt in &program.body {
        let Statement::VariableDeclaration(decl) = stmt else {
            continue;
        };
        for declarator in &decl.declarations {
            collect_from_declarator(declarator, parent_module_id, &mut out);
        }
    }
    out
}

fn collect_from_declarator<'a>(
    declarator: &VariableDeclarator<'a>,
    parent_module_id: ModuleId,
    out: &mut Vec<InnerModule>,
) {
    use oxc_ast::ast::BindingPatternKind;

    let BindingPatternKind::BindingIdentifier(id) = &declarator.id.kind else {
        return;
    };
    if id.name != "__webpack_modules__" {
        return;
    }
    let Some(Expression::ObjectExpression(obj)) = &declarator.init else {
        return;
    };
    for prop in &obj.properties {
        let ObjectPropertyKind::ObjectProperty(p) = prop else {
            continue;
        };
        let key_text = match &p.key {
            PropertyKey::StaticIdentifier(id) => id.name.as_str().to_string(),
            PropertyKey::StringLiteral(s) => s.value.as_str().to_string(),
            PropertyKey::NumericLiteral(n) => format!("{}", n.value),
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
        let source_path_hint = if key_text.starts_with('.') || key_text.starts_with("node_modules/")
        {
            Some(key_text.clone())
        } else {
            None
        };
        out.push(InnerModule {
            virtual_id: format!("webpack5:{}", key_text),
            body_span,
            bundler: BundlerKind::Webpack5,
            source_path_hint,
            parent_module_id,
        });
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
        detect(&parsed.program, ModuleId(7))
    }

    #[test]
    fn detect_webpack5_path_keys_become_source_hints() {
        let src = r#"
            var __webpack_modules__ = {
                "./src/foo.js": (m, e, r) => { e.x = 1; },
                "./src/bar.js": (m, e, r) => { e.y = 2; }
            };
        "#;
        let modules = extract(src);
        assert_eq!(modules.len(), 2);
        for m in &modules {
            assert_eq!(m.bundler, BundlerKind::Webpack5);
            assert!(m.source_path_hint.as_deref().unwrap().starts_with("./src/"));
        }
    }

    #[test]
    fn detect_webpack5_numeric_keys_have_no_path_hint() {
        let src = r#"
            var __webpack_modules__ = {
                42: (m, e, r) => { e.x = 1; }
            };
        "#;
        let modules = extract(src);
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].virtual_id, "webpack5:42");
        assert!(modules[0].source_path_hint.is_none());
    }

    #[test]
    fn detect_webpack5_ignores_other_module_maps() {
        let src = r#"var __not_webpack__ = { "./a": () => {} };"#;
        assert!(extract(src).is_empty());
    }

    #[test]
    fn detect_webpack5_returns_body_span() {
        let src = r#"var __webpack_modules__ = { "./a": (m, e, r) => { var z = 99; e.z = z; } };"#;
        let modules = extract(src);
        let m = &modules[0];
        let body_text = &src[m.body_span.start as usize..m.body_span.end as usize];
        assert!(body_text.starts_with('{'));
        assert!(body_text.ends_with('}'));
        assert!(body_text.contains("var z = 99"));
    }
}
