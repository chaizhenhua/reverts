use oxc_ast::{
    Visit,
    ast::{
        Argument, AssignmentExpression, BindingPatternKind, CallExpression, ExportNamedDeclaration,
        Expression, ImportDeclarationSpecifier, ModuleExportName, ObjectExpression, ObjectProperty,
        ObjectPropertyKind, Program, PropertyKey, Statement,
    },
    visit::walk::{
        walk_assignment_expression, walk_call_expression, walk_export_named_declaration,
        walk_object_property,
    },
};

use crate::expression_identifier;
use crate::identifier::sanitize_identifier;
use crate::rename_apply::{ReadabilityRenameHint, ReadabilityRenameSource};

pub(crate) fn collect_late_readability_rename_hints(
    program: &Program<'_>,
) -> Vec<ReadabilityRenameHint> {
    let mut collector = LateReadabilityRenameCollector { hints: Vec::new() };
    collector.visit_program(program);
    collector.hints
}

struct LateReadabilityRenameCollector {
    hints: Vec<ReadabilityRenameHint>,
}

impl LateReadabilityRenameCollector {
    fn push_hint(&mut self, original: &str, renamed: &str, source: ReadabilityRenameSource) {
        self.hints
            .push(ReadabilityRenameHint::new(original, renamed, source));
    }
}

impl<'a> Visit<'a> for LateReadabilityRenameCollector {
    fn visit_program(&mut self, program: &Program<'a>) {
        collect_import_alias_readability_renames(program, self);
        collect_usage_readability_rename_hints(program, self);
        oxc_ast::visit::walk::walk_program(self, program);
    }

    fn visit_export_named_declaration(&mut self, declaration: &ExportNamedDeclaration<'a>) {
        if declaration.source.is_none() {
            for specifier in &declaration.specifiers {
                let Some(local) = module_export_identifier_name(&specifier.local) else {
                    continue;
                };
                let Some(exported) = module_export_identifier_name(&specifier.exported) else {
                    continue;
                };
                if exported == "default" {
                    continue;
                }
                self.push_hint(
                    local.as_str(),
                    exported.as_str(),
                    ReadabilityRenameSource::ImportExportPublic,
                );
            }
        }
        walk_export_named_declaration(self, declaration);
    }

    fn visit_assignment_expression(&mut self, expression: &AssignmentExpression<'a>) {
        if expression.operator.is_assign() {
            if let Some(exported) = commonjs_export_property_name(&expression.left)
                && let Expression::Identifier(identifier) = &expression.right
            {
                self.push_hint(
                    identifier.name.as_str(),
                    exported.as_str(),
                    ReadabilityRenameSource::CommonJsExport,
                );
            }
            if commonjs_module_exports_target(&expression.left)
                && let Expression::ObjectExpression(object) = &expression.right
            {
                collect_object_export_readability_renames(
                    self,
                    object,
                    ReadabilityRenameSource::CommonJsExport,
                );
            }
        }
        walk_assignment_expression(self, expression);
    }

    fn visit_call_expression(&mut self, expression: &CallExpression<'a>) {
        if let Some((exported, local)) = object_define_property_export_getter(expression) {
            self.push_hint(
                local.as_str(),
                exported.as_str(),
                ReadabilityRenameSource::CommonJsExport,
            );
        }
        walk_call_expression(self, expression);
    }

    fn visit_object_property(&mut self, property: &ObjectProperty<'a>) {
        if !property.computed
            && !property.method
            && !property.shorthand
            && let Some(property_name) = property_key_readability_name(&property.key)
            && let Expression::Identifier(identifier) = &property.value
        {
            self.push_hint(
                identifier.name.as_str(),
                property_name.as_str(),
                ReadabilityRenameSource::ObjectProperty,
            );
        }
        walk_object_property(self, property);
    }
}

fn collect_object_export_readability_renames(
    collector: &mut LateReadabilityRenameCollector,
    object: &ObjectExpression<'_>,
    source: ReadabilityRenameSource,
) {
    for property in &object.properties {
        let ObjectPropertyKind::ObjectProperty(property) = property else {
            continue;
        };
        if !property.computed
            && !property.method
            && !property.shorthand
            && let Some(property_name) = property_key_readability_name(&property.key)
            && let Expression::Identifier(identifier) = &property.value
        {
            collector.push_hint(identifier.name.as_str(), property_name.as_str(), source);
        }
    }
}

fn collect_import_alias_readability_renames(
    program: &Program<'_>,
    collector: &mut LateReadabilityRenameCollector,
) {
    for statement in &program.body {
        let Statement::ImportDeclaration(declaration) = statement else {
            continue;
        };
        let Some(specifiers) = &declaration.specifiers else {
            continue;
        };
        for specifier in specifiers {
            match specifier {
                ImportDeclarationSpecifier::ImportSpecifier(specifier) => {
                    let Some(imported) = module_export_identifier_name(&specifier.imported) else {
                        continue;
                    };
                    if sanitize_identifier(imported.as_str()) != imported {
                        continue;
                    }
                    let local = specifier.local.name.as_str();
                    collector.push_hint(
                        local,
                        imported.as_str(),
                        ReadabilityRenameSource::ImportExportPublic,
                    );
                }
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(specifier) => {
                    let local = specifier.local.name.as_str();
                    if !is_generated_package_namespace_alias(local) {
                        continue;
                    }
                    let Some(namespace) =
                        readable_namespace_name_for_import(declaration.source.value.as_str())
                    else {
                        continue;
                    };
                    collector.push_hint(
                        local,
                        namespace.as_str(),
                        ReadabilityRenameSource::PackageNamespace,
                    );
                }
                ImportDeclarationSpecifier::ImportDefaultSpecifier(_) => {}
            }
        }
    }
}

fn collect_usage_readability_rename_hints(
    program: &Program<'_>,
    collector: &mut LateReadabilityRenameCollector,
) {
    for statement in &program.body {
        let Statement::VariableDeclaration(declaration) = statement else {
            continue;
        };
        if declaration.declare || declaration.declarations.len() != 1 {
            continue;
        }
        let declarator = &declaration.declarations[0];
        if declarator.definite || declarator.id.type_annotation.is_some() || declarator.id.optional
        {
            continue;
        }
        let BindingPatternKind::BindingIdentifier(binding) = &declarator.id.kind else {
            continue;
        };
        let local = binding.name.as_str();
        if !looks_generated_binding_name(local) {
            continue;
        }
        let Some(init) = declarator.init.as_ref() else {
            continue;
        };
        let Some(candidate) = usage_based_name_for_initializer(init) else {
            continue;
        };
        collector.push_hint(
            local,
            candidate.as_str(),
            ReadabilityRenameSource::UsagePattern,
        );
    }
}

fn looks_generated_binding_name(name: &str) -> bool {
    let name = name.trim_start_matches('_');
    name.starts_with('$') || name.chars().count() == 1 || looks_like_letter_number_binding(name)
}

fn looks_like_letter_number_binding(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && !chars.as_str().is_empty()
        && chars.as_str().chars().all(|ch| ch.is_ascii_digit())
}

fn usage_based_name_for_initializer(expression: &Expression<'_>) -> Option<String> {
    match expression {
        Expression::NewExpression(expression) => usage_name_from_constructor(&expression.callee),
        Expression::CallExpression(expression) => usage_name_from_call_callee(&expression.callee),
        _ => None,
    }
}

fn usage_name_from_constructor(callee: &Expression<'_>) -> Option<String> {
    match callee {
        Expression::Identifier(identifier) => lower_type_name(identifier.name.as_str()),
        Expression::StaticMemberExpression(member) => {
            lower_type_name(member.property.name.as_str())
        }
        _ => None,
    }
}

fn usage_name_from_call_callee(callee: &Expression<'_>) -> Option<String> {
    let name = match callee {
        Expression::Identifier(identifier) => identifier.name.as_str(),
        Expression::StaticMemberExpression(member) => member.property.name.as_str(),
        _ => return None,
    };
    let suffix = strip_factory_prefix(name)?;
    lower_type_name(suffix)
}

fn strip_factory_prefix(name: &str) -> Option<&str> {
    for prefix in [
        "create", "make", "build", "get", "load", "init", "use", "open", "read", "parse",
    ] {
        let Some(suffix) = name.strip_prefix(prefix) else {
            continue;
        };
        if suffix
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_uppercase())
        {
            return Some(suffix);
        }
    }
    None
}

fn lower_type_name(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let lowered = if name.chars().all(|ch| ch.is_ascii_uppercase()) {
        name.to_ascii_lowercase()
    } else {
        let mut chars = name.chars();
        let first = chars.next()?;
        let mut lowered = String::new();
        lowered.extend(first.to_lowercase());
        lowered.push_str(chars.as_str());
        lowered
    };
    let sanitized = sanitize_identifier(lowered.as_str());
    if sanitized == lowered {
        Some(lowered)
    } else {
        None
    }
}

pub(crate) fn module_export_identifier_name(name: &ModuleExportName<'_>) -> Option<String> {
    match name {
        ModuleExportName::IdentifierName(identifier) => Some(identifier.name.as_str().to_string()),
        ModuleExportName::IdentifierReference(identifier) => {
            Some(identifier.name.as_str().to_string())
        }
        ModuleExportName::StringLiteral(literal) => Some(literal.value.as_str().to_string()),
    }
}

pub(crate) fn property_key_readability_name(key: &PropertyKey<'_>) -> Option<String> {
    match key {
        PropertyKey::StaticIdentifier(identifier) => Some(identifier.name.as_str().to_string()),
        PropertyKey::StringLiteral(literal) => Some(literal.value.as_str().to_string()),
        _ => None,
    }
}

pub(crate) fn commonjs_export_property_name(
    target: &oxc_ast::ast::AssignmentTarget<'_>,
) -> Option<String> {
    match target {
        oxc_ast::ast::AssignmentTarget::StaticMemberExpression(member) => {
            if expression_is_commonjs_exports_object(&member.object) {
                return Some(member.property.name.as_str().to_string());
            }
        }
        oxc_ast::ast::AssignmentTarget::ComputedMemberExpression(member) => {
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

pub(crate) fn commonjs_module_exports_target(target: &oxc_ast::ast::AssignmentTarget<'_>) -> bool {
    let oxc_ast::ast::AssignmentTarget::StaticMemberExpression(member) = target else {
        return false;
    };
    expression_identifier(&member.object) == Some("module") && member.property.name == "exports"
}

fn expression_is_commonjs_exports_object(expression: &Expression<'_>) -> bool {
    if expression_identifier(expression) == Some("exports") {
        return true;
    }
    let Expression::StaticMemberExpression(member) = expression else {
        return false;
    };
    expression_identifier(&member.object) == Some("module") && member.property.name == "exports"
}

pub(crate) fn object_define_property_export_getter(
    call: &CallExpression<'_>,
) -> Option<(String, String)> {
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
    let exported = argument_string_literal(&call.arguments[1])?;
    let descriptor = argument_object_expression(&call.arguments[2])?;
    let local = descriptor_getter_return_identifier(descriptor)?;
    Some((exported, local))
}

fn argument_is_commonjs_exports_object(argument: &Argument<'_>) -> bool {
    match argument {
        Argument::Identifier(identifier) => identifier.name == "exports",
        Argument::StaticMemberExpression(member) => {
            expression_identifier(&member.object) == Some("module")
                && member.property.name == "exports"
        }
        _ => false,
    }
}

fn argument_string_literal(argument: &Argument<'_>) -> Option<String> {
    let Argument::StringLiteral(literal) = argument else {
        return None;
    };
    Some(literal.value.as_str().to_string())
}

fn argument_object_expression<'a>(argument: &'a Argument<'a>) -> Option<&'a ObjectExpression<'a>> {
    let Argument::ObjectExpression(object) = argument else {
        return None;
    };
    Some(object)
}

fn descriptor_getter_return_identifier(descriptor: &ObjectExpression<'_>) -> Option<String> {
    for property in &descriptor.properties {
        let ObjectPropertyKind::ObjectProperty(property) = property else {
            continue;
        };
        if property.computed
            || property_key_readability_name(&property.key).as_deref() != Some("get")
        {
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

fn single_returned_identifier_from_body(body: &oxc_ast::ast::FunctionBody<'_>) -> Option<String> {
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

fn is_generated_package_namespace_alias(local: &str) -> bool {
    local == "__pkg" || local.starts_with("__pkg_")
}

fn readable_namespace_name_for_import(specifier: &str) -> Option<String> {
    let specifier = specifier
        .split(['?', '#'])
        .next()
        .unwrap_or(specifier)
        .trim();
    if specifier.is_empty() || specifier.starts_with('.') {
        return None;
    }
    let mut words = Vec::new();
    for segment in specifier.split('/') {
        let segment = segment.trim_start_matches('@');
        let mut word = String::new();
        for character in segment.chars() {
            if character.is_ascii_alphanumeric() {
                word.push(character);
            } else if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        }
        if !word.is_empty() {
            words.push(word);
        }
    }
    if words.is_empty() {
        return None;
    }
    let mut candidate = String::new();
    for (index, word) in words.iter().enumerate() {
        if index == 0 {
            candidate.push_str(word);
            continue;
        }
        let mut chars = word.chars();
        let Some(first) = chars.next() else {
            continue;
        };
        candidate.extend(first.to_uppercase());
        candidate.push_str(chars.as_str());
    }
    let sanitized = sanitize_identifier(candidate.as_str());
    if sanitized == "_" {
        None
    } else {
        Some(sanitized)
    }
}
