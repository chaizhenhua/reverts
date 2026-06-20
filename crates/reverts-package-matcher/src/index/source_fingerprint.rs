//! Source-level fingerprinting used by the package matcher to compare a
//! bundle module against package source candidates. Produces a
//! [`SourceFingerprint`] composed of normalized source hashes, function
//! signature hashes, and string/shape anchors collected by walking an OXC
//! AST. The fingerprint is purely structural; no I/O happens here.

use std::collections::BTreeSet;
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::{
    AstKind, Visit,
    ast::{
        ArrowFunctionExpression, BindingPattern, CallExpression, Class, ClassElement, Declaration,
        ExportAllDeclaration, ExportDefaultDeclaration, ExportDefaultDeclarationKind,
        ExportNamedDeclaration, Expression, ImportDeclaration, ImportDeclarationSpecifier,
        MethodDefinitionKind, ObjectExpression, Program, Statement, SwitchStatement,
        TemplateElement,
    },
    visit::walk::{
        walk_assignment_expression, walk_call_expression, walk_class, walk_export_all_declaration,
        walk_export_default_declaration, walk_export_named_declaration, walk_import_declaration,
        walk_object_expression, walk_switch_statement, walk_template_element,
    },
};
use oxc_parser::Parser;
use oxc_span::GetSpan;
use reverts_ir::NormalizationPassId;
use reverts_ir::hash::{
    FNV_OFFSET_BASIS, fnv1a_hex as stable_hash, update_fnv1a as update_stable_hash,
};
use reverts_js::normalize::{apply_to_source, stable_passes};
use reverts_js::{
    JsError, ParseError, ParseGoal, commonjs_create_binding_export_member,
    commonjs_export_property_name, commonjs_module_exports_target, module_export_name,
    object_define_property_export_member, parse_error_message, parse_options_for,
    source_type_candidates, static_property_key_name,
};

use crate::normalize_source;
use crate::package_helpers::normalize_hint_text;
use crate::source::ast_export_helpers::{declaration_binding_names, object_expression_static_keys};
use crate::source::exported_members::{is_identifier_name, is_usable_export_member};

const MIN_STRING_ANCHOR_LEN: usize = 3;
const MIN_REGEX_ANCHOR_LEN: usize = 6;
const MODULE_SOURCE_HASH_ALTERNATE_MAX_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFingerprint {
    pub normalized_source_hash: String,
    pub normalized_source_hashes: BTreeSet<String>,
    pub function_signature_hashes: BTreeSet<String>,
    pub top_level_declaration_hashes: BTreeSet<String>,
    pub string_anchors: BTreeSet<String>,
}

pub fn fingerprint_source(path: &str, source: &str) -> Result<SourceFingerprint, String> {
    let normalized = normalize_source(path, source)?;
    let ast = ast_fingerprint(path, normalized.as_str())?;
    let normalized_source_hash = stable_hash(normalized.as_bytes());
    let mut normalized_source_hashes = BTreeSet::new();
    normalized_source_hashes.insert(normalized_source_hash.clone());
    if normalized.len() <= MODULE_SOURCE_HASH_ALTERNATE_MAX_BYTES {
        for pass in stable_passes() {
            if !module_source_hash_alternate_pass_enabled(pass.id()) {
                continue;
            }
            let Ok(transformed) = apply_to_source(pass.as_ref(), normalized.as_str()) else {
                continue;
            };
            let Ok(renormalized) = normalize_source(path, transformed.as_str()) else {
                continue;
            };
            normalized_source_hashes.insert(stable_hash(renormalized.as_bytes()));
        }
    }
    Ok(SourceFingerprint {
        normalized_source_hash,
        normalized_source_hashes,
        function_signature_hashes: ast.function_signature_hashes,
        top_level_declaration_hashes: ast.top_level_declaration_hashes,
        string_anchors: ast.string_anchors,
    })
}

#[derive(Debug, Default)]
struct AstFingerprint {
    function_signature_hashes: BTreeSet<String>,
    top_level_declaration_hashes: BTreeSet<String>,
    string_anchors: BTreeSet<String>,
    surface_anchors: BTreeSet<String>,
    prototype_members: BTreeSet<String>,
}

fn ast_fingerprint(path: &str, normalized_source: &str) -> Result<AstFingerprint, String> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();
    for source_type in source_type_candidates(Some(Path::new(path)), ParseGoal::TypeScript) {
        let parsed = Parser::new(&allocator, normalized_source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if parsed.errors.is_empty() && !parsed.panicked {
            let mut visitor = FingerprintVisitor {
                source: normalized_source,
                fingerprint: AstFingerprint::default(),
            };
            visitor.record_top_level_declaration_hashes(&parsed.program);
            visitor.visit_program(&parsed.program);
            return Ok(visitor.finish());
        }

        errors.push(ParseError {
            source_type: format!("{source_type:?}"),
            diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
        });
    }

    Err(parse_error_message(
        &JsError::ParseFailed(errors),
        "source could not be parsed",
    ))
}

struct FingerprintVisitor<'s> {
    source: &'s str,
    fingerprint: AstFingerprint,
}

impl<'a> Visit<'a> for FingerprintVisitor<'_> {
    fn enter_node(&mut self, kind: AstKind<'a>) {
        match kind {
            AstKind::Function(function) => self.record_function(
                "function",
                function.r#async,
                function.generator,
                function.params.items.len(),
                function.span.start,
                function.span.end,
            ),
            AstKind::ArrowFunctionExpression(arrow) => self.record_arrow_function(arrow),
            AstKind::StringLiteral(literal) => self.record_string_anchor(literal.value.as_str()),
            AstKind::RegExpLiteral(literal) => self.record_regex_anchor(
                literal.regex.pattern.to_string().as_str(),
                literal.regex.flags.to_string().as_str(),
            ),
            _ => {}
        }
    }

    fn visit_template_element(&mut self, it: &TemplateElement<'a>) {
        if let Some(cooked) = &it.value.cooked {
            self.record_string_anchor(cooked.as_str());
        } else {
            self.record_string_anchor(it.value.raw.as_str());
        }
        walk_template_element(self, it);
    }

    fn visit_import_declaration(&mut self, declaration: &ImportDeclaration<'a>) {
        self.record_import_surface_anchor(declaration);
        walk_import_declaration(self, declaration);
    }

    fn visit_export_named_declaration(&mut self, declaration: &ExportNamedDeclaration<'a>) {
        if let Some(declaration) = &declaration.declaration {
            for binding in declaration_binding_names(declaration) {
                self.record_export_member_anchor(binding);
            }
        }
        for specifier in &declaration.specifiers {
            if let Some(exported) = module_export_name(&specifier.exported) {
                self.record_export_member_anchor(exported);
            }
        }
        self.record_export_surface_anchor(declaration);
        walk_export_named_declaration(self, declaration);
    }

    fn visit_export_default_declaration(&mut self, declaration: &ExportDefaultDeclaration<'a>) {
        match &declaration.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(function) => {
                if let Some(id) = &function.id {
                    self.record_export_member_anchor(id.name.as_str());
                }
            }
            ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                if let Some(id) = &class.id {
                    self.record_export_member_anchor(id.name.as_str());
                }
            }
            _ => {}
        }
        walk_export_default_declaration(self, declaration);
    }

    fn visit_export_all_declaration(&mut self, declaration: &ExportAllDeclaration<'a>) {
        if let Some(exported) = &declaration.exported
            && let Some(binding) = module_export_name(exported)
        {
            self.record_export_member_anchor(binding);
        }
        walk_export_all_declaration(self, declaration);
    }

    fn visit_assignment_expression(&mut self, expression: &oxc_ast::ast::AssignmentExpression<'a>) {
        if expression.operator.is_assign() {
            if let Some(exported) = commonjs_export_property_name(&expression.left) {
                self.record_export_member_anchor(exported.as_str());
            }
            if commonjs_module_exports_target(&expression.left)
                && let Expression::ObjectExpression(object) = &expression.right
            {
                for member in object_expression_static_keys(object) {
                    self.record_export_member_anchor(member.as_str());
                }
            }
            if let Some(member) = prototype_assignment_property_name(&expression.left) {
                self.record_prototype_member_anchor(member.as_str());
            }
        }
        walk_assignment_expression(self, expression);
    }

    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if let Some(exported) = object_define_property_export_member(call) {
            self.record_export_member_anchor(exported.as_str());
        }
        if let Some(exported) = commonjs_create_binding_export_member(call) {
            self.record_export_member_anchor(exported.as_str());
        }
        walk_call_expression(self, call);
    }

    fn visit_object_expression(&mut self, object: &ObjectExpression<'a>) {
        self.record_object_shape_anchor(object);
        walk_object_expression(self, object);
    }

    fn visit_class(&mut self, class: &Class<'a>) {
        self.record_class_shape_anchor(class);
        walk_class(self, class);
    }

    fn visit_switch_statement(&mut self, statement: &SwitchStatement<'a>) {
        self.record_switch_shape_anchor(statement);
        walk_switch_statement(self, statement);
    }
}

impl FingerprintVisitor<'_> {
    fn record_top_level_declaration_hashes(&mut self, program: &Program<'_>) {
        for statement in &program.body {
            let Some(kind) = top_level_declaration_hash_kind(statement) else {
                continue;
            };
            let span = statement.span();
            let Some(source_slice) = self.source.get(span.start as usize..span.end as usize) else {
                continue;
            };
            let hash = stable_hash(source_slice.trim().as_bytes());
            self.fingerprint
                .top_level_declaration_hashes
                .insert(format!("top-level-decl:{kind}:{hash}"));
            if let Some(shape) = top_level_declaration_shape(statement) {
                let shape_hash = stable_hash(shape.as_bytes());
                self.fingerprint
                    .top_level_declaration_hashes
                    .insert(format!("top-level-decl-shape:{kind}:{shape_hash}"));
            }
        }
    }

    fn record_arrow_function(&mut self, arrow: &ArrowFunctionExpression<'_>) {
        self.record_function(
            "arrow",
            arrow.r#async,
            false,
            arrow.params.items.len(),
            arrow.span.start,
            arrow.span.end,
        );
    }

    fn record_function(
        &mut self,
        kind: &str,
        r#async: bool,
        generator: bool,
        parameter_count: usize,
        start: u32,
        end: u32,
    ) {
        let Some(source_slice) = self.source.get(start as usize..end as usize) else {
            return;
        };
        let mut hash = FNV_OFFSET_BASIS;
        update_stable_hash(&mut hash, kind.as_bytes());
        update_stable_hash(&mut hash, b"|async=");
        update_stable_hash(&mut hash, if r#async { b"1" } else { b"0" });
        update_stable_hash(&mut hash, b"|generator=");
        update_stable_hash(&mut hash, if generator { b"1" } else { b"0" });
        update_stable_hash(&mut hash, b"|params=");
        update_stable_hash(&mut hash, parameter_count.to_string().as_bytes());
        update_stable_hash(&mut hash, b"|source=");
        update_stable_hash(&mut hash, source_slice.as_bytes());
        self.fingerprint
            .function_signature_hashes
            .insert(format!("{hash:016x}"));
    }

    fn record_string_anchor(&mut self, value: &str) {
        let trimmed = value.trim();
        if trimmed.len() >= MIN_STRING_ANCHOR_LEN {
            self.fingerprint.string_anchors.insert(trimmed.to_string());
        }
    }

    fn record_regex_anchor(&mut self, pattern: &str, flags: &str) {
        let pattern = pattern.trim();
        if pattern.len() >= MIN_REGEX_ANCHOR_LEN {
            self.fingerprint
                .string_anchors
                .insert(format!("regex:{pattern}/{flags}"));
        }
    }

    fn record_import_surface_anchor(&mut self, declaration: &ImportDeclaration<'_>) {
        let Some(specifiers) = declaration.specifiers.as_ref() else {
            self.fingerprint.surface_anchors.insert(format!(
                "import-surface:{}:side-effect",
                declaration.source.value
            ));
            return;
        };
        let mut parts = BTreeSet::<String>::new();
        for specifier in specifiers {
            match specifier {
                ImportDeclarationSpecifier::ImportDefaultSpecifier(_) => {
                    parts.insert("default".to_string());
                }
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(_) => {
                    parts.insert("namespace".to_string());
                }
                ImportDeclarationSpecifier::ImportSpecifier(specifier) => {
                    if let Some(imported) = module_export_name(&specifier.imported) {
                        parts.insert(format!("named:{imported}"));
                    }
                }
            }
        }
        if !parts.is_empty() {
            self.fingerprint.surface_anchors.insert(format!(
                "import-surface:{}:{}",
                declaration.source.value,
                parts.into_iter().collect::<Vec<_>>().join(",")
            ));
        }
    }

    fn record_export_surface_anchor(&mut self, declaration: &ExportNamedDeclaration<'_>) {
        let mut parts = BTreeSet::<String>::new();
        for specifier in &declaration.specifiers {
            if let Some(exported) = module_export_name(&specifier.exported) {
                parts.insert(exported.to_string());
            }
        }
        if !parts.is_empty() {
            let source = declaration
                .source
                .as_ref()
                .map_or("local".to_string(), |source| source.value.to_string());
            self.fingerprint.surface_anchors.insert(format!(
                "export-surface:{source}:{}",
                parts.into_iter().collect::<Vec<_>>().join(",")
            ));
        }
    }

    fn record_export_member_anchor(&mut self, member: &str) {
        if is_usable_export_member(member) {
            self.fingerprint
                .string_anchors
                .insert(format!("export:{member}"));
        }
    }

    fn record_prototype_member_anchor(&mut self, member: &str) {
        if is_identifier_name(member) {
            self.fingerprint
                .prototype_members
                .insert(member.to_string());
        }
        if is_usable_property_shape_member(member) {
            self.fingerprint
                .string_anchors
                .insert(format!("prototype-member:{member}"));
        }
    }

    fn record_object_shape_anchor(&mut self, object: &ObjectExpression<'_>) {
        let keys = object_expression_static_keys(object)
            .into_iter()
            .filter(|key| is_usable_object_shape_key(key.as_str()))
            .collect::<BTreeSet<_>>();
        if keys.len() < 4 {
            return;
        }
        for key in &keys {
            self.fingerprint
                .string_anchors
                .insert(format!("object-key:{key}"));
        }
        let shape = keys
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(",");
        self.fingerprint
            .string_anchors
            .insert(format!("object-shape:{shape}"));
    }

    fn record_class_shape_anchor(&mut self, class: &Class<'_>) {
        let methods = class_method_shape_members(class);
        if methods.len() < 3 {
            return;
        }
        for method in &methods {
            self.fingerprint
                .string_anchors
                .insert(format!("class-method:{method}"));
        }
        let shape = methods
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(",");
        self.fingerprint
            .string_anchors
            .insert(format!("class-shape:{shape}"));
    }

    fn record_switch_shape_anchor(&mut self, statement: &SwitchStatement<'_>) {
        let labels = switch_statement_shape_labels(statement);
        if labels.len() < 3 {
            return;
        }
        for label in &labels {
            self.fingerprint
                .string_anchors
                .insert(format!("switch-case:{label}"));
        }
        let shape = labels
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(",");
        self.fingerprint
            .string_anchors
            .insert(format!("switch-shape:{shape}"));
    }

    fn finish(mut self) -> AstFingerprint {
        if self.fingerprint.prototype_members.len() >= 3 {
            let members = self
                .fingerprint
                .prototype_members
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(",");
            self.fingerprint
                .string_anchors
                .insert(format!("prototype-shape:{members}"));
        }
        self.fingerprint
    }
}

fn top_level_declaration_hash_kind(statement: &Statement<'_>) -> Option<&'static str> {
    match statement {
        Statement::FunctionDeclaration(_) => Some("function"),
        Statement::ClassDeclaration(_) => Some("class"),
        Statement::VariableDeclaration(_) => Some("variable"),
        Statement::TSTypeAliasDeclaration(_) => Some("type_alias"),
        Statement::TSInterfaceDeclaration(_) => Some("interface"),
        Statement::TSEnumDeclaration(_) => Some("enum"),
        Statement::TSModuleDeclaration(_) => Some("module"),
        Statement::TSImportEqualsDeclaration(_) => Some("import_equals"),
        Statement::ExportNamedDeclaration(declaration) if declaration.declaration.is_some() => {
            Some("export_named_decl")
        }
        Statement::ExportDefaultDeclaration(_) => Some("export_default_decl"),
        _ => None,
    }
}

fn top_level_declaration_shape(statement: &Statement<'_>) -> Option<String> {
    match statement {
        Statement::FunctionDeclaration(function) => Some(function_shape(function)),
        Statement::ClassDeclaration(class) => Some(class_declaration_shape(class)),
        Statement::VariableDeclaration(declaration) => Some(format!(
            "variable:{:?}:{}",
            declaration.kind,
            declaration
                .declarations
                .iter()
                .map(|declarator| {
                    let binding = binding_pattern_shape(&declarator.id);
                    let init = declarator
                        .init
                        .as_ref()
                        .map(expression_shape)
                        .unwrap_or("none");
                    format!("{binding}={init}")
                })
                .collect::<Vec<_>>()
                .join(",")
        )),
        Statement::TSTypeAliasDeclaration(_) => Some("type_alias".to_string()),
        Statement::TSInterfaceDeclaration(_) => Some("interface".to_string()),
        Statement::TSEnumDeclaration(declaration) => {
            Some(format!("enum:members={}", declaration.members.len()))
        }
        Statement::TSModuleDeclaration(_) => Some("module".to_string()),
        Statement::TSImportEqualsDeclaration(_) => Some("import_equals".to_string()),
        Statement::ExportNamedDeclaration(declaration) => declaration
            .declaration
            .as_ref()
            .map(|declaration| format!("export_named:{}", declaration_shape(declaration))),
        Statement::ExportDefaultDeclaration(declaration) => Some(match &declaration.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(function) => {
                format!("export_default:{}", function_shape(function))
            }
            ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                format!("export_default:{}", class_declaration_shape(class))
            }
            ExportDefaultDeclarationKind::TSInterfaceDeclaration(_) => {
                "export_default:interface".to_string()
            }
            declaration => declaration.as_expression().map_or_else(
                || "export_default:other".to_string(),
                |expression| format!("export_default:expr:{}", expression_shape(expression)),
            ),
        }),
        _ => None,
    }
}

fn declaration_shape(declaration: &Declaration<'_>) -> String {
    match declaration {
        Declaration::VariableDeclaration(declaration) => format!(
            "variable:{:?}:{}",
            declaration.kind,
            declaration
                .declarations
                .iter()
                .map(|declarator| {
                    let binding = binding_pattern_shape(&declarator.id);
                    let init = declarator
                        .init
                        .as_ref()
                        .map(expression_shape)
                        .unwrap_or("none");
                    format!("{binding}={init}")
                })
                .collect::<Vec<_>>()
                .join(",")
        ),
        Declaration::FunctionDeclaration(function) => function_shape(function),
        Declaration::ClassDeclaration(class) => class_declaration_shape(class),
        Declaration::TSTypeAliasDeclaration(_) => "type_alias".to_string(),
        Declaration::TSInterfaceDeclaration(_) => "interface".to_string(),
        Declaration::TSEnumDeclaration(declaration) => {
            format!("enum:members={}", declaration.members.len())
        }
        Declaration::TSModuleDeclaration(_) => "module".to_string(),
        Declaration::TSImportEqualsDeclaration(_) => "import_equals".to_string(),
    }
}

fn function_shape(function: &oxc_ast::ast::Function<'_>) -> String {
    let body_statement_count = function
        .body
        .as_ref()
        .map(|body| body.statements.len())
        .unwrap_or_default();
    format!(
        "function:async={}:generator={}:params={}:body={body_statement_count}",
        u8::from(function.r#async),
        u8::from(function.generator),
        function.params.items.len()
    )
}

fn class_declaration_shape(class: &Class<'_>) -> String {
    let method_count = class
        .body
        .body
        .iter()
        .filter(|element| matches!(element, ClassElement::MethodDefinition(_)))
        .count();
    let property_count = class.body.body.len().saturating_sub(method_count);
    format!(
        "class:members={}:methods={method_count}:properties={property_count}",
        class.body.body.len()
    )
}

fn binding_pattern_shape(pattern: &BindingPattern<'_>) -> &'static str {
    match &pattern.kind {
        oxc_ast::ast::BindingPatternKind::BindingIdentifier(_) => "id",
        oxc_ast::ast::BindingPatternKind::ObjectPattern(_) => "object",
        oxc_ast::ast::BindingPatternKind::ArrayPattern(_) => "array",
        oxc_ast::ast::BindingPatternKind::AssignmentPattern(_) => "assignment",
    }
}

fn expression_shape(expression: &Expression<'_>) -> &'static str {
    match expression {
        Expression::BooleanLiteral(_) => "boolean",
        Expression::NullLiteral(_) => "null",
        Expression::NumericLiteral(_) => "number",
        Expression::BigIntLiteral(_) => "bigint",
        Expression::RegExpLiteral(_) => "regexp",
        Expression::StringLiteral(_) => "string",
        Expression::TemplateLiteral(_) => "template",
        Expression::Identifier(_) => "identifier",
        Expression::ArrayExpression(_) => "array",
        Expression::ArrowFunctionExpression(_) => "arrow",
        Expression::AssignmentExpression(_) => "assignment",
        Expression::AwaitExpression(_) => "await",
        Expression::BinaryExpression(_) => "binary",
        Expression::CallExpression(_) => "call",
        Expression::ChainExpression(_) => "chain",
        Expression::ClassExpression(_) => "class",
        Expression::ConditionalExpression(_) => "conditional",
        Expression::FunctionExpression(_) => "function",
        Expression::ImportExpression(_) => "import",
        Expression::LogicalExpression(_) => "logical",
        Expression::NewExpression(_) => "new",
        Expression::ObjectExpression(_) => "object",
        Expression::ParenthesizedExpression(expression) => expression_shape(&expression.expression),
        Expression::SequenceExpression(_) => "sequence",
        Expression::TaggedTemplateExpression(_) => "tagged_template",
        Expression::ThisExpression(_) => "this",
        Expression::UnaryExpression(_) => "unary",
        Expression::UpdateExpression(_) => "update",
        Expression::YieldExpression(_) => "yield",
        Expression::JSXElement(_) => "jsx_element",
        Expression::JSXFragment(_) => "jsx_fragment",
        Expression::TSAsExpression(expression) => expression_shape(&expression.expression),
        Expression::TSSatisfiesExpression(expression) => expression_shape(&expression.expression),
        Expression::TSTypeAssertion(expression) => expression_shape(&expression.expression),
        Expression::TSNonNullExpression(expression) => expression_shape(&expression.expression),
        Expression::TSInstantiationExpression(expression) => {
            expression_shape(&expression.expression)
        }
        Expression::StaticMemberExpression(_) => "static_member",
        Expression::ComputedMemberExpression(_) => "computed_member",
        Expression::PrivateFieldExpression(_) => "private_field",
        _ => "other",
    }
}

fn prototype_assignment_property_name(
    target: &oxc_ast::ast::AssignmentTarget<'_>,
) -> Option<String> {
    match target {
        oxc_ast::ast::AssignmentTarget::StaticMemberExpression(member) => {
            expression_is_prototype_member(&member.object)
                .then(|| member.property.name.as_str().to_string())
        }
        oxc_ast::ast::AssignmentTarget::ComputedMemberExpression(member) => {
            if !expression_is_prototype_member(&member.object) {
                return None;
            }
            let Expression::StringLiteral(property) = &member.expression else {
                return None;
            };
            Some(property.value.as_str().to_string())
        }
        _ => None,
    }
}

fn expression_is_prototype_member(expression: &Expression<'_>) -> bool {
    let Expression::StaticMemberExpression(member) = expression else {
        return false;
    };
    member.property.name == "prototype"
}

fn is_usable_property_shape_member(member: &str) -> bool {
    !matches!(member, "constructor" | "__proto__" | "prototype") && is_identifier_name(member)
}

fn is_usable_object_shape_key(key: &str) -> bool {
    if !is_identifier_name(key) {
        return false;
    }
    let normalized = normalize_hint_text(key);
    normalized.len() >= 3
        && !matches!(
            normalized.as_str(),
            "key"
                | "keys"
                | "map"
                | "get"
                | "set"
                | "has"
                | "add"
                | "run"
                | "main"
                | "init"
                | "name"
                | "type"
                | "types"
                | "value"
                | "values"
                | "index"
                | "default"
        )
}

fn class_method_shape_members(class: &Class<'_>) -> BTreeSet<String> {
    class
        .body
        .body
        .iter()
        .filter_map(|element| {
            let ClassElement::MethodDefinition(method) = element else {
                return None;
            };
            if method.computed {
                return None;
            }
            let name = static_property_key_name(&method.key)?;
            if !is_usable_class_shape_member(name.as_str()) {
                return None;
            }
            let kind = match method.kind {
                MethodDefinitionKind::Constructor => return None,
                MethodDefinitionKind::Method => "method",
                MethodDefinitionKind::Get => "get",
                MethodDefinitionKind::Set => "set",
            };
            let scope = if method.r#static {
                "static"
            } else {
                "instance"
            };
            Some(format!("{scope}:{kind}:{name}"))
        })
        .collect()
}

fn is_usable_class_shape_member(member: &str) -> bool {
    if !is_identifier_name(member) || member.starts_with('_') {
        return false;
    }
    let normalized = normalize_hint_text(member);
    normalized.len() >= 3
        && !matches!(
            normalized.as_str(),
            "constructor"
                | "prototype"
                | "default"
                | "get"
                | "set"
                | "has"
                | "map"
                | "key"
                | "keys"
                | "add"
                | "run"
                | "main"
                | "init"
                | "name"
                | "type"
                | "types"
                | "value"
                | "values"
                | "index"
        )
}

fn switch_statement_shape_labels(statement: &SwitchStatement<'_>) -> BTreeSet<String> {
    statement
        .cases
        .iter()
        .filter_map(|case| {
            let test = case.test.as_ref()?;
            switch_case_static_label(test)
        })
        .filter(|label| is_usable_switch_shape_label(label.as_str()))
        .collect()
}

fn switch_case_static_label(expression: &Expression<'_>) -> Option<String> {
    match expression {
        Expression::StringLiteral(literal) => Some(literal.value.as_str().trim().to_string()),
        Expression::TemplateLiteral(literal) if literal.expressions.is_empty() => literal
            .quasis
            .first()
            .map(|element| {
                element
                    .value
                    .cooked
                    .as_ref()
                    .unwrap_or(&element.value.raw)
                    .as_str()
            })
            .map(str::trim)
            .map(ToOwned::to_owned),
        _ => None,
    }
}

fn is_usable_switch_shape_label(label: &str) -> bool {
    if label.len() > 64 {
        return false;
    }
    let normalized = normalize_hint_text(label);
    normalized.len() >= 3
        && !matches!(
            normalized.as_str(),
            "get"
                | "set"
                | "has"
                | "map"
                | "key"
                | "keys"
                | "add"
                | "run"
                | "main"
                | "init"
                | "name"
                | "type"
                | "types"
                | "value"
                | "values"
                | "index"
                | "default"
                | "true"
                | "false"
        )
}

fn module_source_hash_alternate_pass_enabled(pass: NormalizationPassId) -> bool {
    matches!(
        pass,
        NormalizationPassId::TsRuntimeErased
            | NormalizationPassId::JsxRuntimeNormalized
            | NormalizationPassId::BundlerWrapperUnwrapped
            | NormalizationPassId::HelperIdentityInlined
            | NormalizationPassId::ExportBoundaryNormalized
            | NormalizationPassId::CommonJsExportBoundaryNormalized
            | NormalizationPassId::BooleanUndefinedCanonicalised
            | NormalizationPassId::ComputedToStaticMember
            | NormalizationPassId::VoidZeroToUndefinedGuarded
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_surface_shape_ignores_named_specifier_order() {
        let left = ast_fingerprint(
            "a.ts",
            "import { a, b as c } from 'pkg'; export const x = a;",
        )
        .expect("left fingerprint");
        let right = ast_fingerprint(
            "b.ts",
            "import { b as c, a } from 'pkg'; export const x = a;",
        )
        .expect("right fingerprint");
        assert!(
            left.surface_anchors
                .contains("import-surface:pkg:named:a,named:b")
        );
        assert_eq!(left.surface_anchors, right.surface_anchors);
    }

    #[test]
    fn export_surface_shape_ignores_specifier_order() {
        let left = ast_fingerprint("a.ts", "const a = 1; const b = 2; export { a, b };")
            .expect("left fingerprint");
        let right = ast_fingerprint("b.ts", "const a = 1; const b = 2; export { b, a };")
            .expect("right fingerprint");
        assert!(left.surface_anchors.contains("export-surface:local:a,b"));
        assert_eq!(left.surface_anchors, right.surface_anchors);
    }

    #[test]
    fn top_level_declaration_hashes_ignore_statement_order() {
        let left = fingerprint_source(
            "a.ts",
            "const a = 1;\nfunction run() { return a; }\nexport class Widget {}\n",
        )
        .expect("left fingerprint");
        let right = fingerprint_source(
            "b.ts",
            "export class Widget {}\nfunction run() { return a; }\nconst a = 1;\n",
        )
        .expect("right fingerprint");
        assert_eq!(
            left.top_level_declaration_hashes,
            right.top_level_declaration_hashes
        );
        assert_eq!(left.top_level_declaration_hashes.len(), 6);
    }
}
