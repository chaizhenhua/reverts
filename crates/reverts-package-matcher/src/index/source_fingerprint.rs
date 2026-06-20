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
        Argument, ArrowFunctionExpression, BindingPattern, BlockStatement, CallExpression, Class,
        ClassElement, Declaration, ExportAllDeclaration, ExportDefaultDeclaration,
        ExportDefaultDeclarationKind, ExportNamedDeclaration, Expression, FunctionBody,
        ImportDeclaration, ImportDeclarationSpecifier, ImportExpression, MethodDefinitionKind,
        ObjectExpression, Program, Statement, SwitchStatement, TemplateElement,
    },
    visit::walk::{
        walk_assignment_expression, walk_block_statement, walk_call_expression, walk_class,
        walk_export_all_declaration, walk_export_default_declaration,
        walk_export_named_declaration, walk_function_body, walk_import_declaration,
        walk_import_expression, walk_object_expression, walk_switch_statement,
        walk_template_element,
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
    pub import_export_surface_hashes: BTreeSet<String>,
    pub class_member_hashes: BTreeSet<String>,
    pub statement_window_hashes: BTreeSet<String>,
    pub block_branch_hashes: BTreeSet<String>,
    pub pq_gram_hashes: BTreeSet<String>,
    pub wl_hashes: BTreeSet<String>,
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
        import_export_surface_hashes: ast.import_export_surface_hashes,
        class_member_hashes: ast.class_member_hashes,
        statement_window_hashes: ast.statement_window_hashes,
        block_branch_hashes: ast.block_branch_hashes,
        pq_gram_hashes: ast.pq_gram_hashes,
        wl_hashes: ast.wl_hashes,
        string_anchors: ast.string_anchors,
    })
}

#[derive(Debug, Default)]
struct AstFingerprint {
    function_signature_hashes: BTreeSet<String>,
    top_level_declaration_hashes: BTreeSet<String>,
    import_export_surface_hashes: BTreeSet<String>,
    class_member_hashes: BTreeSet<String>,
    statement_window_hashes: BTreeSet<String>,
    block_branch_hashes: BTreeSet<String>,
    pq_gram_hashes: BTreeSet<String>,
    wl_hashes: BTreeSet<String>,
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
            visitor.record_statement_window_hashes(&parsed.program.body, "program");
            visitor.record_block_branch_hashes(&parsed.program.body, "program");
            visitor.record_pq_grams(&parsed.program);
            visitor.record_wl_hashes(&parsed.program);
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
        self.record_export_default_surface_anchor(declaration);
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
        self.record_export_all_surface_anchor(declaration);
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
                self.record_commonjs_export_surface_anchor(exported.as_str());
            }
            if commonjs_module_exports_target(&expression.left)
                && let Expression::ObjectExpression(object) = &expression.right
            {
                let members = object_expression_static_keys(object);
                self.record_commonjs_module_exports_object_surface_anchor(&members);
                for member in members {
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
        if expression_identifier(&call.callee) == Some("require")
            && let Some(Argument::StringLiteral(source)) = call.arguments.first()
        {
            self.record_require_surface_anchor(source.value.as_str(), "require");
        }
        if let Some(exported) = object_define_property_export_member(call) {
            self.record_export_member_anchor(exported.as_str());
            self.record_commonjs_export_surface_anchor(exported.as_str());
        }
        if let Some(exported) = commonjs_create_binding_export_member(call) {
            self.record_export_member_anchor(exported.as_str());
            self.record_commonjs_export_surface_anchor(exported.as_str());
        }
        walk_call_expression(self, call);
    }

    fn visit_import_expression(&mut self, expression: &ImportExpression<'a>) {
        if let Expression::StringLiteral(source) = &expression.source {
            self.record_require_surface_anchor(source.value.as_str(), "dynamic-import");
        }
        walk_import_expression(self, expression);
    }

    fn visit_function_body(&mut self, body: &FunctionBody<'a>) {
        self.record_statement_window_hashes(&body.statements, "function");
        self.record_block_branch_hashes(&body.statements, "function");
        walk_function_body(self, body);
    }

    fn visit_block_statement(&mut self, block: &BlockStatement<'a>) {
        self.record_statement_window_hashes(&block.body, "block");
        self.record_block_branch_hashes(&block.body, "block");
        walk_block_statement(self, block);
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

    fn record_statement_window_hashes(&mut self, statements: &[Statement<'_>], scope: &str) {
        let shapes = statements.iter().map(statement_shape).collect::<Vec<_>>();
        for width in [2usize, 3] {
            if shapes.len() < width {
                continue;
            }
            for window in shapes.windows(width) {
                let hash = stable_hash(window.join("|").as_bytes());
                self.fingerprint
                    .statement_window_hashes
                    .insert(format!("statement-window:{scope}:{width}:{hash}"));
            }
        }
    }

    fn record_block_branch_hashes(&mut self, statements: &[Statement<'_>], scope: &str) {
        if statements.is_empty() {
            return;
        }
        let mut bag = statements.iter().map(statement_shape).collect::<Vec<_>>();
        let sequence_hash = stable_hash(bag.join("|").as_bytes());
        self.fingerprint
            .block_branch_hashes
            .insert(format!("block-seq:{scope}:{}:{sequence_hash}", bag.len()));
        bag.sort();
        let bag_hash = stable_hash(bag.join("|").as_bytes());
        self.fingerprint
            .block_branch_hashes
            .insert(format!("block-bag:{scope}:{}:{bag_hash}", bag.len()));
    }

    fn record_pq_grams(&mut self, program: &Program<'_>) {
        let tree = program_tree(program);
        collect_pq_grams(&tree, &mut Vec::new(), &mut self.fingerprint.pq_gram_hashes);
    }

    fn record_wl_hashes(&mut self, program: &Program<'_>) {
        let tree = program_tree(program);
        collect_wl_hashes(&tree, &mut self.fingerprint.wl_hashes);
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
            self.fingerprint
                .surface_anchors
                .insert("import-surface-shape:side-effect".to_string());
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
            let shape = import_surface_shape(&parts);
            self.fingerprint.surface_anchors.insert(format!(
                "import-surface:{}:{}",
                declaration.source.value,
                parts.into_iter().collect::<Vec<_>>().join(",")
            ));
            self.fingerprint
                .surface_anchors
                .insert(format!("import-surface-shape:{shape}"));
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
            let shape = export_surface_shape(&parts);
            let source = declaration
                .source
                .as_ref()
                .map_or("local".to_string(), |source| source.value.to_string());
            self.fingerprint.surface_anchors.insert(format!(
                "export-surface:{source}:{}",
                parts.into_iter().collect::<Vec<_>>().join(",")
            ));
            self.fingerprint
                .surface_anchors
                .insert(format!("export-surface-shape:{shape}"));
        }
    }

    fn record_export_default_surface_anchor(
        &mut self,
        _declaration: &ExportDefaultDeclaration<'_>,
    ) {
        self.fingerprint
            .surface_anchors
            .insert("export-surface:local:default".to_string());
        self.fingerprint
            .surface_anchors
            .insert("export-surface-shape:default".to_string());
    }

    fn record_export_all_surface_anchor(&mut self, declaration: &ExportAllDeclaration<'_>) {
        let export_kind = declaration
            .exported
            .as_ref()
            .and_then(module_export_name)
            .map_or("all".to_string(), |exported| {
                format!("namespace:{exported}")
            });
        self.fingerprint.surface_anchors.insert(format!(
            "export-all-surface:{}:{export_kind}",
            declaration.source.value
        ));
        self.fingerprint
            .surface_anchors
            .insert(format!("export-surface-shape:{export_kind}"));
    }

    fn record_require_surface_anchor(&mut self, source: &str, kind: &str) {
        self.fingerprint
            .surface_anchors
            .insert(format!("import-surface:{source}:{kind}"));
        self.fingerprint
            .surface_anchors
            .insert(format!("import-surface-shape:{kind}"));
    }

    fn record_commonjs_export_surface_anchor(&mut self, member: &str) {
        if !is_usable_export_member(member) {
            return;
        }
        self.fingerprint
            .surface_anchors
            .insert(format!("export-surface:local:{member}"));
        self.fingerprint
            .surface_anchors
            .insert("export-surface-shape:named:1".to_string());
    }

    fn record_commonjs_module_exports_object_surface_anchor(&mut self, members: &[String]) {
        let parts = members
            .iter()
            .filter(|member| is_usable_export_member(member.as_str()))
            .cloned()
            .collect::<BTreeSet<_>>();
        if parts.is_empty() {
            self.fingerprint
                .surface_anchors
                .insert("export-surface-shape:commonjs-default".to_string());
            return;
        }
        let shape = export_surface_shape(&parts);
        self.fingerprint.surface_anchors.insert(format!(
            "export-surface:local:{}",
            parts
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(",")
        ));
        self.fingerprint
            .surface_anchors
            .insert(format!("export-surface-shape:{shape}"));
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
        self.record_member_multiset_hash("object", &keys);
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
        let methods = class_member_shape_members(class);
        self.record_member_multiset_hash("class", &methods);
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
        let hash = stable_hash(shape.as_bytes());
        self.fingerprint
            .block_branch_hashes
            .insert(format!("switch-branch:{}:{hash}", labels.len()));
    }

    fn record_member_multiset_hash(&mut self, kind: &str, members: &BTreeSet<String>) {
        if members.len() < 2 {
            return;
        }
        let shape = members
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(",");
        let hash = stable_hash(shape.as_bytes());
        self.fingerprint
            .class_member_hashes
            .insert(format!("{kind}-member-multiset:{}:{hash}", members.len()));
    }

    fn finish(mut self) -> AstFingerprint {
        for anchor in &self.fingerprint.surface_anchors {
            let hash = stable_hash(anchor.as_bytes());
            self.fingerprint
                .import_export_surface_hashes
                .insert(format!("import-export-surface:{hash}"));
        }
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
            let hash = stable_hash(members.as_bytes());
            self.fingerprint.class_member_hashes.insert(format!(
                "prototype-member-multiset:{}:{hash}",
                self.fingerprint.prototype_members.len()
            ));
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

fn statement_shape(statement: &Statement<'_>) -> String {
    match statement {
        Statement::BlockStatement(block) => format!("block:{}", block.body.len()),
        Statement::BreakStatement(_) => "break".to_string(),
        Statement::ContinueStatement(_) => "continue".to_string(),
        Statement::DebuggerStatement(_) => "debugger".to_string(),
        Statement::DoWhileStatement(_) => "do_while".to_string(),
        Statement::EmptyStatement(_) => "empty".to_string(),
        Statement::ExpressionStatement(statement) => {
            format!("expression:{}", expression_shape(&statement.expression))
        }
        Statement::ForInStatement(_) => "for_in".to_string(),
        Statement::ForOfStatement(_) => "for_of".to_string(),
        Statement::ForStatement(_) => "for".to_string(),
        Statement::IfStatement(statement) => format!(
            "if:{}:{}",
            statement_shape(&statement.consequent),
            statement.alternate.as_ref().map_or("no_alt", |alternate| {
                if matches!(alternate, Statement::IfStatement(_)) {
                    "else_if"
                } else {
                    "else"
                }
            })
        ),
        Statement::LabeledStatement(_) => "label".to_string(),
        Statement::ReturnStatement(statement) => statement.argument.as_ref().map_or_else(
            || "return:none".to_string(),
            |argument| format!("return:{}", expression_shape(argument)),
        ),
        Statement::SwitchStatement(statement) => format!("switch:cases={}", statement.cases.len()),
        Statement::ThrowStatement(statement) => {
            format!("throw:{}", expression_shape(&statement.argument))
        }
        Statement::TryStatement(statement) => format!(
            "try:catch={}:finally={}",
            u8::from(statement.handler.is_some()),
            u8::from(statement.finalizer.is_some())
        ),
        Statement::WhileStatement(_) => "while".to_string(),
        Statement::WithStatement(_) => "with".to_string(),
        Statement::FunctionDeclaration(function) => function_shape(function),
        Statement::ClassDeclaration(class) => class_declaration_shape(class),
        Statement::VariableDeclaration(declaration) => format!(
            "variable:{:?}:{}",
            declaration.kind,
            declaration.declarations.len()
        ),
        Statement::TSTypeAliasDeclaration(_) => "type_alias".to_string(),
        Statement::TSInterfaceDeclaration(_) => "interface".to_string(),
        Statement::TSEnumDeclaration(declaration) => {
            format!("enum:members={}", declaration.members.len())
        }
        Statement::TSModuleDeclaration(_) => "module".to_string(),
        Statement::TSImportEqualsDeclaration(_) => "import_equals".to_string(),
        Statement::ImportDeclaration(declaration) => {
            let specifier_count = declaration
                .specifiers
                .as_ref()
                .map_or(0, |items| items.len());
            format!("import:specifiers={specifier_count}")
        }
        Statement::ExportAllDeclaration(_) => "export_all".to_string(),
        Statement::ExportDefaultDeclaration(declaration) => format!(
            "export_default:{}",
            declaration
                .declaration
                .as_expression()
                .map(expression_shape)
                .unwrap_or("declaration")
        ),
        Statement::ExportNamedDeclaration(declaration) => format!(
            "export_named:specifiers={}:decl={}",
            declaration.specifiers.len(),
            u8::from(declaration.declaration.is_some())
        ),
        Statement::TSExportAssignment(_) => "ts_export_assignment".to_string(),
        Statement::TSNamespaceExportDeclaration(_) => "ts_namespace_export".to_string(),
    }
}

#[derive(Debug)]
struct TreeNode {
    label: String,
    children: Vec<TreeNode>,
}

const PQ_GRAM_P: usize = 2;
const PQ_GRAM_Q: usize = 3;
const PQ_PAD: &str = "*";
const WL_ROUNDS: usize = 2;

fn program_tree(program: &Program<'_>) -> TreeNode {
    TreeNode {
        label: "program".to_string(),
        children: program.body.iter().map(statement_tree).collect(),
    }
}

fn collect_pq_grams(node: &TreeNode, ancestors: &mut Vec<String>, output: &mut BTreeSet<String>) {
    let mut ancestor_window = vec![PQ_PAD.to_string(); PQ_GRAM_P];
    let start = ancestors.len().saturating_sub(PQ_GRAM_P);
    for (index, ancestor) in ancestors[start..].iter().enumerate() {
        let slot = PQ_GRAM_P - (ancestors.len() - start) + index;
        ancestor_window[slot] = ancestor.clone();
    }

    let mut child_labels = vec![PQ_PAD.to_string(); PQ_GRAM_Q - 1];
    child_labels.extend(node.children.iter().map(|child| child.label.clone()));
    child_labels.extend(std::iter::repeat_n(PQ_PAD.to_string(), PQ_GRAM_Q - 1));
    for window in child_labels.windows(PQ_GRAM_Q) {
        let gram = format!(
            "{}|{}|{}",
            ancestor_window.join("/"),
            node.label,
            window.join("/")
        );
        output.insert(format!("pq-gram:{:016x}", stable_hash_u64(gram.as_bytes())));
    }

    ancestors.push(node.label.clone());
    for child in &node.children {
        collect_pq_grams(child, ancestors, output);
    }
    ancestors.pop();
}

fn stable_hash_u64(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    update_stable_hash(&mut hash, bytes);
    hash
}

fn collect_wl_hashes(node: &TreeNode, output: &mut BTreeSet<String>) -> Vec<u64> {
    let child_rounds = node
        .children
        .iter()
        .map(|child| collect_wl_hashes(child, output))
        .collect::<Vec<_>>();
    let mut rounds = Vec::with_capacity(WL_ROUNDS + 1);
    let round0 = stable_hash_u64(format!("wl0:{}", node.label).as_bytes());
    rounds.push(round0);
    for round in 1..=WL_ROUNDS {
        let mut neighbor_labels = child_rounds
            .iter()
            .filter_map(|labels| labels.get(round - 1).copied())
            .collect::<Vec<_>>();
        neighbor_labels.sort_unstable();
        let mut payload = format!("wl{round}:{}:{}", node.label, node.children.len());
        for label in neighbor_labels {
            payload.push(':');
            payload.push_str(format!("{label:016x}").as_str());
        }
        let hash = stable_hash_u64(payload.as_bytes());
        output.insert(format!("wl-round{round}:{hash:016x}"));
        rounds.push(hash);
    }
    rounds
}

fn statement_tree(statement: &Statement<'_>) -> TreeNode {
    let mut node = TreeNode {
        label: statement_shape(statement),
        children: Vec::new(),
    };
    match statement {
        Statement::BlockStatement(block) => {
            node.children.extend(block.body.iter().map(statement_tree));
        }
        Statement::DoWhileStatement(statement) => {
            node.children.push(statement_tree(&statement.body));
        }
        Statement::ExpressionStatement(statement) => {
            node.children.push(expression_tree(&statement.expression));
        }
        Statement::ForInStatement(statement) => {
            node.children.push(expression_tree(&statement.right));
            node.children.push(statement_tree(&statement.body));
        }
        Statement::ForOfStatement(statement) => {
            node.children.push(expression_tree(&statement.right));
            node.children.push(statement_tree(&statement.body));
        }
        Statement::ForStatement(statement) => {
            if let Some(test) = &statement.test {
                node.children.push(expression_tree(test));
            }
            if let Some(update) = &statement.update {
                node.children.push(expression_tree(update));
            }
            node.children.push(statement_tree(&statement.body));
        }
        Statement::IfStatement(statement) => {
            node.children.push(expression_tree(&statement.test));
            node.children.push(statement_tree(&statement.consequent));
            if let Some(alternate) = &statement.alternate {
                node.children.push(statement_tree(alternate));
            }
        }
        Statement::LabeledStatement(statement) => {
            node.children.push(statement_tree(&statement.body));
        }
        Statement::ReturnStatement(statement) => {
            if let Some(argument) = &statement.argument {
                node.children.push(expression_tree(argument));
            }
        }
        Statement::SwitchStatement(statement) => {
            node.children.push(expression_tree(&statement.discriminant));
            for case in &statement.cases {
                let mut case_node = TreeNode {
                    label: if case.test.is_some() {
                        "case".to_string()
                    } else {
                        "default".to_string()
                    },
                    children: Vec::new(),
                };
                if let Some(test) = &case.test {
                    case_node.children.push(expression_tree(test));
                }
                case_node
                    .children
                    .extend(case.consequent.iter().map(statement_tree));
                node.children.push(case_node);
            }
        }
        Statement::ThrowStatement(statement) => {
            node.children.push(expression_tree(&statement.argument));
        }
        Statement::TryStatement(statement) => {
            node.children
                .extend(statement.block.body.iter().map(statement_tree));
            if let Some(handler) = &statement.handler {
                let mut catch_node = TreeNode {
                    label: "catch".to_string(),
                    children: Vec::new(),
                };
                catch_node
                    .children
                    .extend(handler.body.body.iter().map(statement_tree));
                node.children.push(catch_node);
            }
            if let Some(finalizer) = &statement.finalizer {
                let mut finally_node = TreeNode {
                    label: "finally".to_string(),
                    children: Vec::new(),
                };
                finally_node
                    .children
                    .extend(finalizer.body.iter().map(statement_tree));
                node.children.push(finally_node);
            }
        }
        Statement::WhileStatement(statement) => {
            node.children.push(expression_tree(&statement.test));
            node.children.push(statement_tree(&statement.body));
        }
        Statement::WithStatement(statement) => {
            node.children.push(expression_tree(&statement.object));
            node.children.push(statement_tree(&statement.body));
        }
        Statement::FunctionDeclaration(function) => {
            if let Some(body) = &function.body {
                node.children
                    .extend(body.statements.iter().map(statement_tree));
            }
        }
        Statement::ClassDeclaration(class) => {
            node.children
                .extend(
                    class_member_shape_members(class)
                        .into_iter()
                        .map(|label| TreeNode {
                            label,
                            children: Vec::new(),
                        }),
                );
        }
        Statement::VariableDeclaration(declaration) => {
            for declarator in &declaration.declarations {
                let mut declarator_node = TreeNode {
                    label: format!("binding:{}", binding_pattern_shape(&declarator.id)),
                    children: Vec::new(),
                };
                if let Some(init) = &declarator.init {
                    declarator_node.children.push(expression_tree(init));
                }
                node.children.push(declarator_node);
            }
        }
        Statement::ExportNamedDeclaration(declaration) => {
            if let Some(declaration) = &declaration.declaration {
                node.children.push(declaration_tree(declaration));
            }
        }
        Statement::ExportDefaultDeclaration(declaration) => {
            match &declaration.declaration {
                ExportDefaultDeclarationKind::FunctionDeclaration(function) => {
                    if let Some(body) = &function.body {
                        node.children
                            .extend(body.statements.iter().map(statement_tree));
                    }
                }
                ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                    node.children
                        .extend(class_member_shape_members(class).into_iter().map(|label| {
                            TreeNode {
                                label,
                                children: Vec::new(),
                            }
                        }));
                }
                declaration => {
                    if let Some(expression) = declaration.as_expression() {
                        node.children.push(expression_tree(expression));
                    }
                }
            }
        }
        _ => {}
    }
    node
}

fn declaration_tree(declaration: &Declaration<'_>) -> TreeNode {
    match declaration {
        Declaration::VariableDeclaration(declaration) => {
            let mut node = TreeNode {
                label: format!(
                    "variable:{:?}:{}",
                    declaration.kind,
                    declaration.declarations.len()
                ),
                children: Vec::new(),
            };
            for declarator in &declaration.declarations {
                if let Some(init) = &declarator.init {
                    node.children.push(expression_tree(init));
                }
            }
            node
        }
        Declaration::FunctionDeclaration(function) => {
            let mut node = TreeNode {
                label: function_shape(function),
                children: Vec::new(),
            };
            if let Some(body) = &function.body {
                node.children
                    .extend(body.statements.iter().map(statement_tree));
            }
            node
        }
        Declaration::ClassDeclaration(class) => TreeNode {
            label: class_declaration_shape(class),
            children: class_member_shape_members(class)
                .into_iter()
                .map(|label| TreeNode {
                    label,
                    children: Vec::new(),
                })
                .collect(),
        },
        _ => TreeNode {
            label: declaration_shape(declaration),
            children: Vec::new(),
        },
    }
}

fn expression_tree(expression: &Expression<'_>) -> TreeNode {
    TreeNode {
        label: format!("expr:{}", expression_shape(expression)),
        children: Vec::new(),
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

fn class_member_shape_members(class: &Class<'_>) -> BTreeSet<String> {
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

fn expression_identifier<'a>(expression: &'a Expression<'a>) -> Option<&'a str> {
    match expression {
        Expression::Identifier(identifier) => Some(identifier.name.as_str()),
        _ => None,
    }
}

fn import_surface_shape(parts: &BTreeSet<String>) -> String {
    let default = usize::from(parts.contains("default"));
    let namespace = usize::from(parts.contains("namespace"));
    let named = parts
        .iter()
        .filter(|part| part.starts_with("named:"))
        .count();
    format!("default:{default};namespace:{namespace};named:{named}")
}

fn export_surface_shape(parts: &BTreeSet<String>) -> String {
    format!("named:{}", parts.len())
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

    #[test]
    fn surface_hashes_ignore_import_export_specifier_order() {
        let left = fingerprint_source(
            "a.ts",
            "import { a, b as c } from 'pkg'; const x = a; export { x as one, c as two };",
        )
        .expect("left fingerprint");
        let right = fingerprint_source(
            "b.ts",
            "import { b as c, a } from 'pkg'; const x = a; export { c as two, x as one };",
        )
        .expect("right fingerprint");
        assert_eq!(
            left.import_export_surface_hashes,
            right.import_export_surface_hashes
        );
        assert!(!left.import_export_surface_hashes.is_empty());
    }

    #[test]
    fn surface_hashes_capture_cjs_require_and_exports() {
        let fingerprint = fingerprint_source(
            "a.cjs",
            "const fs = require('fs'); exports.read = () => fs.readFileSync; module.exports.extra = 1;",
        )
        .expect("fingerprint");
        let ast = ast_fingerprint(
            "a.cjs",
            "const fs = require('fs'); exports.read = () => fs.readFileSync; module.exports.extra = 1;",
        )
        .expect("ast fingerprint");

        assert!(
            ast.surface_anchors.contains("import-surface:fs:require"),
            "require calls should participate in import/export surface"
        );
        assert!(
            ast.surface_anchors.contains("export-surface:local:read"),
            "exports.member should participate in export surface"
        );
        assert!(
            ast.surface_anchors.contains("export-surface:local:extra"),
            "module.exports.member should participate in export surface"
        );
        assert!(!fingerprint.import_export_surface_hashes.is_empty());
    }

    #[test]
    fn surface_shape_hashes_abstract_over_source_and_order() {
        let left = fingerprint_source("a.ts", "import { a, b } from './local'; export { a, b };")
            .expect("left fingerprint");
        let right = fingerprint_source("b.ts", "import { y, x } from 'pkg'; export { x, y };")
            .expect("right fingerprint");

        assert!(
            !left
                .import_export_surface_hashes
                .is_disjoint(&right.import_export_surface_hashes),
            "weak import/export surface shape should survive specifier source/name differences"
        );
    }

    #[test]
    fn member_multiset_hashes_ignore_class_member_order() {
        let left = fingerprint_source("a.ts", "class Widget { start() {} stop() {} reset() {} }\n")
            .expect("left fingerprint");
        let right =
            fingerprint_source("b.ts", "class Widget { reset() {} start() {} stop() {} }\n")
                .expect("right fingerprint");
        assert_eq!(left.class_member_hashes, right.class_member_hashes);
        assert!(!left.class_member_hashes.is_empty());
    }

    #[test]
    fn statement_window_and_block_branch_hashes_capture_local_shape() {
        let fingerprint = fingerprint_source(
            "a.ts",
            "function run(x) { const y = x + 1; log(y); if (x) { return 'yes'; } else { throw new Error('no'); } }\n",
        )
        .expect("fingerprint");
        assert!(!fingerprint.statement_window_hashes.is_empty());
        assert!(!fingerprint.block_branch_hashes.is_empty());
    }

    #[test]
    fn pq_grams_ignore_local_names_and_capture_statement_tree() {
        let left = fingerprint_source(
            "a.ts",
            "function run(x) { const y = x + 1; if (y) { return 'yes'; } return 'no'; }\n",
        )
        .expect("left fingerprint");
        let right = fingerprint_source(
            "b.ts",
            "function run(value) { const result = value + 1; if (result) { return 'yes'; } return 'no'; }\n",
        )
        .expect("right fingerprint");
        assert_eq!(left.pq_gram_hashes, right.pq_gram_hashes);
        assert!(!left.pq_gram_hashes.is_empty());
    }

    #[test]
    fn wl_hashes_ignore_local_names_and_refine_context() {
        let left = fingerprint_source(
            "a.ts",
            "function run(x) { const y = x + 1; if (y) { return 'yes'; } return 'no'; }\n",
        )
        .expect("left fingerprint");
        let right = fingerprint_source(
            "b.ts",
            "function run(value) { const result = value + 1; if (result) { return 'yes'; } return 'no'; }\n",
        )
        .expect("right fingerprint");
        let different = fingerprint_source(
            "c.ts",
            "function run(value) { while (value) { value--; } return 'no'; }\n",
        )
        .expect("different fingerprint");
        assert_eq!(left.wl_hashes, right.wl_hashes);
        assert!(!left.wl_hashes.is_empty());
        assert_ne!(left.wl_hashes, different.wl_hashes);
    }
}
