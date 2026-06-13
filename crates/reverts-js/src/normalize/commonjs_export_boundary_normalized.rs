use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, AssignmentTarget, Expression, FunctionBody, ObjectExpression, ObjectPropertyKind,
    Program, PropertyKey, Statement,
};
use reverts_ir::NormalizationPassId;

use super::NormalizationPass;

/// Removes CommonJS export-boundary boilerplate when it only re-exports a
/// local binding. This is intentionally conservative: it drops statements such
/// as `exports.foo = foo` and `Object.defineProperty(exports, "__esModule", …)`,
/// but it does not drop inline `module.exports = function () { … }` bodies where
/// the export assignment is also the implementation.
pub struct CommonJsExportBoundaryNormalized;

impl NormalizationPass for CommonJsExportBoundaryNormalized {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::CommonJsExportBoundaryNormalized
    }

    fn version(&self) -> u32 {
        1
    }

    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut new_body = oxc_allocator::Vec::with_capacity_in(program.body.len(), alloc);
        for stmt in program.body.drain(..) {
            if !is_commonjs_export_boundary_statement(&stmt) {
                new_body.push(stmt);
            }
        }
        program.body = new_body;
    }
}

fn is_commonjs_export_boundary_statement(statement: &Statement<'_>) -> bool {
    let Statement::ExpressionStatement(statement) = statement else {
        return false;
    };
    match &statement.expression {
        Expression::AssignmentExpression(assignment) => {
            assignment.operator.is_assign()
                && is_commonjs_export_target(&assignment.left)
                && is_reexported_local_binding(&assignment.right)
        }
        Expression::CallExpression(call) => is_commonjs_define_property_export_boundary(call),
        _ => false,
    }
}

fn is_commonjs_export_target(target: &AssignmentTarget<'_>) -> bool {
    match target {
        AssignmentTarget::StaticMemberExpression(member) => {
            expression_is_commonjs_exports_object(&member.object)
                || (expression_identifier(&member.object) == Some("module")
                    && member.property.name.as_str() == "exports")
        }
        AssignmentTarget::ComputedMemberExpression(member) => {
            expression_is_commonjs_exports_object(&member.object)
                && matches!(&member.expression, Expression::StringLiteral(_))
        }
        _ => false,
    }
}

fn expression_is_commonjs_exports_object(expression: &Expression<'_>) -> bool {
    if expression_identifier(expression) == Some("exports") {
        return true;
    }
    let Expression::StaticMemberExpression(member) = expression else {
        return false;
    };
    expression_identifier(&member.object) == Some("module")
        && member.property.name.as_str() == "exports"
}

fn is_reexported_local_binding(expression: &Expression<'_>) -> bool {
    match expression {
        Expression::Identifier(_) => true,
        Expression::StaticMemberExpression(member) => is_reexported_local_binding(&member.object),
        Expression::ComputedMemberExpression(member) => {
            is_reexported_local_binding(&member.object)
                && matches!(
                    &member.expression,
                    Expression::StringLiteral(_) | Expression::NumericLiteral(_)
                )
        }
        _ => false,
    }
}

fn is_commonjs_define_property_export_boundary(call: &oxc_ast::ast::CallExpression<'_>) -> bool {
    let Expression::StaticMemberExpression(callee) = &call.callee else {
        return false;
    };
    if expression_identifier(&callee.object) != Some("Object")
        || callee.property.name.as_str() != "defineProperty"
        || call.arguments.len() < 2
    {
        return false;
    }
    if !argument_is_commonjs_exports_object(&call.arguments[0]) {
        return false;
    }
    let Argument::StringLiteral(key) = &call.arguments[1] else {
        return false;
    };
    if key.value.as_str() == "__esModule" {
        return true;
    }
    let Some(descriptor) = call.arguments.get(2).and_then(argument_object_expression) else {
        return false;
    };
    descriptor_is_reexport_boundary(descriptor)
}

fn argument_is_commonjs_exports_object(argument: &Argument<'_>) -> bool {
    match argument {
        Argument::Identifier(identifier) => identifier.name.as_str() == "exports",
        Argument::StaticMemberExpression(member) => {
            expression_identifier(&member.object) == Some("module")
                && member.property.name.as_str() == "exports"
        }
        _ => false,
    }
}

fn argument_object_expression<'a>(argument: &'a Argument<'a>) -> Option<&'a ObjectExpression<'a>> {
    let Argument::ObjectExpression(object) = argument else {
        return None;
    };
    Some(object)
}

fn descriptor_is_reexport_boundary(descriptor: &ObjectExpression<'_>) -> bool {
    let mut found_reexport_binding = false;
    for property in &descriptor.properties {
        let ObjectPropertyKind::ObjectProperty(property) = property else {
            return false;
        };
        if property.computed {
            return false;
        }
        let Some(key) = property_key_name(&property.key) else {
            return false;
        };
        match key {
            "enumerable" | "configurable" => {}
            "get" => {
                if !invokable_returns_reexported_binding(&property.value) {
                    return false;
                }
                found_reexport_binding = true;
            }
            "value" => {
                if !is_reexported_local_binding(&property.value) {
                    return false;
                }
                found_reexport_binding = true;
            }
            _ => return false,
        }
    }
    found_reexport_binding
}

fn property_key_name<'a>(key: &'a PropertyKey<'a>) -> Option<&'a str> {
    match key {
        PropertyKey::StaticIdentifier(identifier) => Some(identifier.name.as_str()),
        PropertyKey::StringLiteral(literal) => Some(literal.value.as_str()),
        _ => None,
    }
}

fn invokable_returns_reexported_binding(expression: &Expression<'_>) -> bool {
    match expression {
        Expression::FunctionExpression(function) => function
            .body
            .as_deref()
            .is_some_and(single_returned_reexported_binding_from_body),
        Expression::ArrowFunctionExpression(arrow) => {
            single_returned_reexported_binding_from_body(&arrow.body)
        }
        _ => false,
    }
}

fn single_returned_reexported_binding_from_body(body: &FunctionBody<'_>) -> bool {
    if !body.directives.is_empty() || body.statements.len() != 1 {
        return false;
    }
    let Statement::ReturnStatement(statement) = &body.statements[0] else {
        return false;
    };
    statement
        .argument
        .as_ref()
        .is_some_and(is_reexported_local_binding)
}

fn expression_identifier<'a>(expression: &'a Expression<'a>) -> Option<&'a str> {
    let Expression::Identifier(identifier) = expression else {
        return None;
    };
    Some(identifier.name.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn drops_commonjs_reexport_assignment() {
        let src = "function add(a, b) { return a + b; }\nexports.add = add;";
        let out = apply_to_source(&CommonJsExportBoundaryNormalized, src).expect("parses");

        assert!(out.contains("function add"));
        assert!(!out.contains("exports.add"), "got: {out}");
    }

    #[test]
    fn keeps_inline_module_exports_implementation() {
        let src = "module.exports = function add(a, b) { return a + b; };";
        let out = apply_to_source(&CommonJsExportBoundaryNormalized, src).expect("parses");

        assert!(out.contains("module.exports"), "got: {out}");
        assert!(out.contains("function add"), "got: {out}");
    }

    #[test]
    fn drops_es_module_marker() {
        let src = "Object.defineProperty(exports, \"__esModule\", { value: true });\nconst v = 1;";
        let out = apply_to_source(&CommonJsExportBoundaryNormalized, src).expect("parses");

        assert!(!out.contains("__esModule"), "got: {out}");
        assert!(out.contains("const v"), "got: {out}");
    }

    #[test]
    fn drops_define_property_getter_reexport() {
        let src = r#"
            function add(a, b) { return a + b; }
            Object.defineProperty(exports, "add", {
              enumerable: true,
              get: function () { return add; }
            });
        "#;
        let out = apply_to_source(&CommonJsExportBoundaryNormalized, src).expect("parses");

        assert!(out.contains("function add"), "got: {out}");
        assert!(!out.contains("defineProperty"), "got: {out}");
    }

    #[test]
    fn keeps_define_property_inline_implementation() {
        let src = r#"
            Object.defineProperty(exports, "add", {
              value: function add(a, b) { return a + b; }
            });
        "#;
        let out = apply_to_source(&CommonJsExportBoundaryNormalized, src).expect("parses");

        assert!(out.contains("defineProperty"), "got: {out}");
        assert!(out.contains("function add"), "got: {out}");
    }
}
