//! AST-shape helpers shared by the export-member collector and the
//! source-fingerprint visitor. These walk OXC AST nodes that describe
//! ES module / CommonJS export shapes and surface the static identifier
//! names that participate in the public surface.

use oxc_ast::ast::{
    BindingPattern, BindingPatternKind, Declaration, ObjectExpression, ObjectPropertyKind,
    PropertyKey,
};

#[must_use]
pub(crate) fn declaration_binding_names<'a>(declaration: &'a Declaration<'a>) -> Vec<&'a str> {
    match declaration {
        Declaration::VariableDeclaration(variable) => variable
            .declarations
            .iter()
            .flat_map(|declarator| binding_pattern_names(&declarator.id))
            .collect(),
        Declaration::FunctionDeclaration(function) => function
            .id
            .as_ref()
            .map(|id| vec![id.name.as_str()])
            .unwrap_or_default(),
        Declaration::ClassDeclaration(class) => class
            .id
            .as_ref()
            .map(|id| vec![id.name.as_str()])
            .unwrap_or_default(),
        Declaration::TSTypeAliasDeclaration(declaration) => vec![declaration.id.name.as_str()],
        Declaration::TSInterfaceDeclaration(declaration) => vec![declaration.id.name.as_str()],
        Declaration::TSEnumDeclaration(declaration) => vec![declaration.id.name.as_str()],
        Declaration::TSModuleDeclaration(declaration) => vec![declaration.id.name().as_str()],
        Declaration::TSImportEqualsDeclaration(declaration) => vec![declaration.id.name.as_str()],
    }
}

#[must_use]
pub(crate) fn binding_pattern_names<'a>(pattern: &'a BindingPattern<'a>) -> Vec<&'a str> {
    let mut names = Vec::new();
    collect_binding_pattern_names(pattern, &mut names);
    names
}

pub(crate) fn collect_binding_pattern_names<'a>(
    pattern: &'a BindingPattern<'a>,
    names: &mut Vec<&'a str>,
) {
    match &pattern.kind {
        BindingPatternKind::BindingIdentifier(identifier) => names.push(identifier.name.as_str()),
        BindingPatternKind::AssignmentPattern(pattern) => {
            collect_binding_pattern_names(&pattern.left, names);
        }
        BindingPatternKind::ObjectPattern(pattern) => {
            for property in &pattern.properties {
                collect_binding_pattern_names(&property.value, names);
            }
            if let Some(rest) = &pattern.rest {
                collect_binding_pattern_names(&rest.argument, names);
            }
        }
        BindingPatternKind::ArrayPattern(pattern) => {
            for element in pattern.elements.iter().flatten() {
                collect_binding_pattern_names(element, names);
            }
            if let Some(rest) = &pattern.rest {
                collect_binding_pattern_names(&rest.argument, names);
            }
        }
    }
}

#[must_use]
pub(crate) fn object_expression_static_keys(object: &ObjectExpression<'_>) -> Vec<String> {
    object
        .properties
        .iter()
        .filter_map(|property| {
            let ObjectPropertyKind::ObjectProperty(property) = property else {
                return None;
            };
            if property.computed {
                return None;
            }
            property_key_name(&property.key)
        })
        .collect()
}

#[must_use]
pub(crate) fn property_key_name(key: &PropertyKey<'_>) -> Option<String> {
    match key {
        PropertyKey::StaticIdentifier(identifier) => Some(identifier.name.as_str().to_string()),
        PropertyKey::StringLiteral(literal) => Some(literal.value.as_str().to_string()),
        _ => None,
    }
}
