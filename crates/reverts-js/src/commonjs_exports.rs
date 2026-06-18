//! Shared AST helpers for static CommonJS / ESM export boundary shapes.
//!
//! These helpers intentionally stay in `reverts-js` so downstream crates do
//! not duplicate OXC AST predicates for the same JavaScript export semantics.

use oxc_ast::ast::{
    Argument, AssignmentTarget, CallExpression, Expression, FunctionBody, ModuleExportName,
    ObjectExpression, ObjectPropertyKind, PropertyKey, Statement,
};

use crate::expression_identifier;

#[must_use]
pub fn module_export_name<'a>(name: &'a ModuleExportName<'a>) -> Option<&'a str> {
    match name {
        ModuleExportName::IdentifierName(identifier) => Some(identifier.name.as_str()),
        ModuleExportName::IdentifierReference(identifier) => Some(identifier.name.as_str()),
        ModuleExportName::StringLiteral(literal) => Some(literal.value.as_str()),
    }
}

#[must_use]
pub fn module_export_identifier_name(name: &ModuleExportName<'_>) -> Option<String> {
    module_export_name(name).map(ToOwned::to_owned)
}

#[must_use]
pub fn static_property_key_name_ref<'a>(key: &'a PropertyKey<'a>) -> Option<&'a str> {
    match key {
        PropertyKey::StaticIdentifier(identifier) => Some(identifier.name.as_str()),
        PropertyKey::StringLiteral(literal) => Some(literal.value.as_str()),
        _ => None,
    }
}

#[must_use]
pub fn static_or_private_property_key_name_ref<'a>(key: &'a PropertyKey<'a>) -> Option<&'a str> {
    match key {
        PropertyKey::PrivateIdentifier(identifier) => Some(identifier.name.as_str()),
        _ => static_property_key_name_ref(key),
    }
}

#[must_use]
pub fn static_property_key_name(key: &PropertyKey<'_>) -> Option<String> {
    static_property_key_name_ref(key).map(ToOwned::to_owned)
}

#[must_use]
pub fn commonjs_export_property_name(target: &AssignmentTarget<'_>) -> Option<String> {
    match target {
        AssignmentTarget::StaticMemberExpression(member) => {
            if expression_is_commonjs_exports_object(&member.object) {
                return Some(member.property.name.as_str().to_string());
            }
        }
        AssignmentTarget::ComputedMemberExpression(member) => {
            if expression_is_commonjs_exports_object(&member.object)
                && let Expression::StringLiteral(property) = &member.expression
            {
                return Some(property.value.as_str().to_string());
            }
        }
        _ => {}
    }
    None
}

#[must_use]
pub fn commonjs_module_exports_target(target: &AssignmentTarget<'_>) -> bool {
    let AssignmentTarget::StaticMemberExpression(member) = target else {
        return false;
    };
    expression_identifier(&member.object) == Some("module") && member.property.name == "exports"
}

#[must_use]
pub fn expression_is_commonjs_exports_object(expression: &Expression<'_>) -> bool {
    if expression_identifier(expression) == Some("exports") {
        return true;
    }
    let Expression::StaticMemberExpression(member) = expression else {
        return false;
    };
    expression_identifier(&member.object) == Some("module") && member.property.name == "exports"
}

#[must_use]
pub fn object_define_property_export_getter(call: &CallExpression<'_>) -> Option<(String, String)> {
    let Expression::StaticMemberExpression(callee) = &call.callee else {
        return None;
    };
    if expression_identifier(&callee.object) != Some("Object")
        || callee.property.name != "defineProperty"
        || call.arguments.len() < 3
    {
        return None;
    }
    if !argument_is_commonjs_exports_object(&call.arguments[0]) {
        return None;
    }
    let exported = argument_string_literal_owned(&call.arguments[1])?;
    let descriptor = argument_object_expression(&call.arguments[2])?;
    let local = descriptor_getter_return_identifier(descriptor)?;
    Some((exported, local))
}

#[must_use]
pub fn object_define_property_export_member(call: &CallExpression<'_>) -> Option<String> {
    let Expression::StaticMemberExpression(callee) = &call.callee else {
        return None;
    };
    if expression_identifier(&callee.object) != Some("Object")
        || callee.property.name != "defineProperty"
        || call.arguments.len() < 2
    {
        return None;
    }
    if !argument_is_commonjs_exports_object(&call.arguments[0]) {
        return None;
    }
    argument_string_literal_owned(&call.arguments[1])
}

#[must_use]
pub fn commonjs_create_binding_export_member(call: &CallExpression<'_>) -> Option<String> {
    if expression_identifier(&call.callee) != Some("__createBinding") || call.arguments.len() < 3 {
        return None;
    }
    if !argument_is_commonjs_exports_object(&call.arguments[0]) {
        return None;
    }
    argument_string_literal_owned(&call.arguments[2])
}

#[must_use]
pub fn argument_is_commonjs_exports_object(argument: &Argument<'_>) -> bool {
    match argument {
        Argument::Identifier(identifier) => identifier.name == "exports",
        Argument::StaticMemberExpression(member) => {
            expression_identifier(&member.object) == Some("module")
                && member.property.name == "exports"
        }
        _ => false,
    }
}

#[must_use]
pub fn argument_object_expression<'a>(
    argument: &'a Argument<'a>,
) -> Option<&'a ObjectExpression<'a>> {
    let Argument::ObjectExpression(object) = argument else {
        return None;
    };
    Some(object)
}

fn argument_string_literal_owned(argument: &Argument<'_>) -> Option<String> {
    let Argument::StringLiteral(literal) = argument else {
        return None;
    };
    Some(literal.value.as_str().to_string())
}

fn descriptor_getter_return_identifier(descriptor: &ObjectExpression<'_>) -> Option<String> {
    for property in &descriptor.properties {
        let ObjectPropertyKind::ObjectProperty(property) = property else {
            continue;
        };
        if property.computed || static_property_key_name(&property.key).as_deref() != Some("get") {
            continue;
        }
        if let Some(identifier) = returned_identifier_from_invokable(&property.value) {
            return Some(identifier);
        }
    }
    None
}

fn returned_identifier_from_invokable(expression: &Expression<'_>) -> Option<String> {
    match expression {
        Expression::ArrowFunctionExpression(arrow) => {
            single_returned_identifier_from_body(&arrow.body)
        }
        Expression::FunctionExpression(function) => function
            .body
            .as_deref()
            .and_then(single_returned_identifier_from_body),
        _ => None,
    }
}

fn single_returned_identifier_from_body(body: &FunctionBody<'_>) -> Option<String> {
    if !body.directives.is_empty() || body.statements.len() != 1 {
        return None;
    }
    let Statement::ReturnStatement(statement) = &body.statements[0] else {
        return None;
    };
    let Some(Expression::Identifier(identifier)) = &statement.argument else {
        return None;
    };
    Some(identifier.name.as_str().to_string())
}
