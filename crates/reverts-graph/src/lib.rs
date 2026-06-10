use std::collections::{BTreeMap, BTreeSet};

use oxc_allocator::Allocator;
use oxc_ast::{
    Visit,
    ast::{
        Argument, ArrowFunctionExpression, AssignmentTarget, BindingPattern, BindingPatternKind,
        CallExpression, Class, ComputedMemberExpression, Expression, Function, FunctionType,
        NewExpression, StaticMemberExpression, VariableDeclarator,
    },
    visit::walk::{
        walk_arrow_function_expression, walk_call_expression, walk_class,
        walk_computed_member_expression, walk_function, walk_new_expression,
        walk_static_member_expression, walk_variable_declarator,
    },
};
use oxc_parser::{ParseOptions, Parser};
use oxc_syntax::{
    operator::{AssignmentOperator, LogicalOperator},
    scope::ScopeFlags,
};
use reverts_input::{InputBundle, ModuleDependencyTarget, ModuleInput};
use reverts_ir::{BindingConstraint, BindingConstraintKind, BindingName, DefUseGraph, ModuleId};
use reverts_js::{JsError, ParseError, ParseGoal, source_type_candidates};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevertsGraph {
    modules: BTreeMap<ModuleId, ModuleInput>,
    definitions: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    def_use: DefUseGraph,
    import_export: ImportExportGraph,
    ast_facts: Vec<AstFact>,
    ast_errors: Vec<AstFactError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AstFact {
    pub module_id: ModuleId,
    pub binding: Option<BindingName>,
    pub kind: AstFactKind,
}

impl AstFact {
    #[must_use]
    pub fn definition(module_id: ModuleId, binding: impl Into<String>) -> Self {
        Self {
            module_id,
            binding: Some(BindingName::new(binding)),
            kind: AstFactKind::Definition,
        }
    }

    #[must_use]
    pub fn read(module_id: ModuleId, binding: impl Into<String>) -> Self {
        Self {
            module_id,
            binding: Some(BindingName::new(binding)),
            kind: AstFactKind::Read,
        }
    }

    #[must_use]
    pub fn constraint(
        module_id: ModuleId,
        binding: impl Into<String>,
        kind: BindingConstraintKind,
    ) -> Self {
        Self {
            module_id,
            binding: Some(BindingName::new(binding)),
            kind: AstFactKind::BindingConstraint(kind),
        }
    }

    #[must_use]
    pub fn wrapper_region(module_id: ModuleId, kind: AstWrapperKind) -> Self {
        Self {
            module_id,
            binding: None,
            kind: AstFactKind::WrapperRegion(kind),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AstFactKind {
    Definition,
    Read,
    BindingConstraint(BindingConstraintKind),
    WrapperRegion(AstWrapperKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AstWrapperKind {
    FunctionIife,
    ArrowIife,
    EnumIife,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AstFactError {
    pub module_id: ModuleId,
    pub path: String,
    pub message: String,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AstFactExtractor;

impl RevertsGraph {
    #[must_use]
    pub fn from_input(input: &InputBundle) -> Self {
        let modules = input
            .modules
            .iter()
            .map(|module| (module.id, module.clone()))
            .collect::<BTreeMap<_, _>>();

        let mut definitions: BTreeMap<ModuleId, BTreeSet<BindingName>> = BTreeMap::new();
        let mut def_use = DefUseGraph::default();
        for symbol in &input.symbols {
            def_use.define(symbol.module_id, symbol.name.clone());
            definitions
                .entry(symbol.module_id)
                .or_default()
                .insert(BindingName::new(symbol.name.clone()));
        }

        let mut import_export = ImportExportGraph::default();
        for dependency in &input.dependencies {
            match &dependency.target {
                ModuleDependencyTarget::Module(module_id) => {
                    import_export.record_module_import(dependency.from_module_id, *module_id);
                }
                ModuleDependencyTarget::Package { specifier } => {
                    import_export
                        .record_package_import(dependency.from_module_id, specifier.clone());
                }
            }
        }

        let mut ast_facts = Vec::new();
        let mut ast_errors = Vec::new();
        for module in &input.modules {
            let Some(source) = input.module_source_slice(module.id) else {
                continue;
            };
            match AstFactExtractor.extract(module, source.source_file_path, source.source) {
                Ok(facts) => {
                    for fact in facts {
                        apply_ast_fact(&mut definitions, &mut def_use, &fact);
                        ast_facts.push(fact);
                    }
                }
                Err(message) => ast_errors.push(AstFactError {
                    module_id: module.id,
                    path: source.source_file_path.to_string(),
                    message,
                }),
            }
        }

        Self {
            modules,
            definitions,
            def_use,
            import_export,
            ast_facts,
            ast_errors,
        }
    }

    pub fn record_read(&mut self, module_id: ModuleId, binding: impl Into<String>) {
        self.def_use.read(module_id, binding);
    }

    pub fn record_constraint(
        &mut self,
        module_id: ModuleId,
        binding: impl Into<String>,
        kind: BindingConstraintKind,
    ) {
        self.def_use
            .constrain(BindingConstraint::new(module_id, binding, kind));
    }

    #[must_use]
    pub fn module(&self, module_id: ModuleId) -> Option<&ModuleInput> {
        self.modules.get(&module_id)
    }

    #[must_use]
    pub fn definitions_for(&self, module_id: ModuleId) -> Vec<BindingName> {
        self.definitions
            .get(&module_id)
            .map(|bindings| bindings.iter().cloned().collect())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn def_use(&self) -> &DefUseGraph {
        &self.def_use
    }

    #[must_use]
    pub fn import_export(&self) -> &ImportExportGraph {
        &self.import_export
    }

    #[must_use]
    pub fn ast_facts(&self) -> &[AstFact] {
        &self.ast_facts
    }

    #[must_use]
    pub fn ast_errors(&self) -> &[AstFactError] {
        &self.ast_errors
    }
}

fn apply_ast_fact(
    definitions: &mut BTreeMap<ModuleId, BTreeSet<BindingName>>,
    def_use: &mut DefUseGraph,
    fact: &AstFact,
) {
    match &fact.kind {
        AstFactKind::Definition => {
            if let Some(binding) = &fact.binding {
                def_use.define(fact.module_id, binding.as_str());
                definitions
                    .entry(fact.module_id)
                    .or_default()
                    .insert(binding.clone());
            }
        }
        AstFactKind::Read => {
            if let Some(binding) = &fact.binding {
                def_use.read(fact.module_id, binding.as_str());
            }
        }
        AstFactKind::BindingConstraint(kind) => {
            if let Some(binding) = &fact.binding {
                def_use.constrain(BindingConstraint::new(
                    fact.module_id,
                    binding.as_str(),
                    *kind,
                ));
            }
        }
        AstFactKind::WrapperRegion(_) => {}
    }
}

impl AstFactExtractor {
    pub fn extract(
        self,
        module: &ModuleInput,
        source_path: &str,
        source: &str,
    ) -> Result<Vec<AstFact>, String> {
        let allocator = Allocator::default();
        let mut errors = Vec::new();

        for source_type in source_type_candidates(
            Some(std::path::Path::new(source_path)),
            ParseGoal::TypeScript,
        ) {
            let parsed = Parser::new(&allocator, source, source_type)
                .with_options(parse_options_for(source_type))
                .parse();
            if parsed.errors.is_empty() && !parsed.panicked {
                let mut visitor = AstFactVisitor {
                    module_id: module.id,
                    facts: Vec::new(),
                    function_depth: 0,
                };
                visitor.visit_program(&parsed.program);
                return Ok(visitor.facts);
            }
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
        }

        Err(parse_error_message(&JsError::ParseFailed(errors)))
    }
}

fn parse_options_for(source_type: oxc_span::SourceType) -> ParseOptions {
    ParseOptions {
        allow_return_outside_function: source_type.is_script(),
        ..Default::default()
    }
}

fn parse_error_message(error: &JsError) -> String {
    match error {
        JsError::ParseFailed(errors) => errors.first().map_or_else(
            || "source could not be parsed".to_string(),
            |error| {
                let diagnostic = error
                    .diagnostics
                    .first()
                    .map_or("no diagnostic", String::as_str);
                format!(
                    "source could not be parsed as {}: {diagnostic}",
                    error.source_type
                )
            },
        ),
    }
}

struct AstFactVisitor {
    module_id: ModuleId,
    facts: Vec<AstFact>,
    function_depth: usize,
}

impl<'a> Visit<'a> for AstFactVisitor {
    fn visit_identifier_reference(&mut self, it: &oxc_ast::ast::IdentifierReference<'a>) {
        self.read(it.name.as_str());
    }

    fn visit_variable_declarator(&mut self, it: &VariableDeclarator<'a>) {
        if self.function_depth == 0
            && let Some(binding) = binding_pattern_name(&it.id)
        {
            self.definition(binding);
        }
        walk_variable_declarator(self, it);
    }

    fn visit_class(&mut self, it: &Class<'a>) {
        if self.function_depth == 0
            && let Some(id) = &it.id
        {
            self.definition(id.name.as_str());
            self.constraint(id.name.as_str(), BindingConstraintKind::ClassDeclaration);
        }
        walk_class(self, it);
    }

    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        if self.function_depth == 0
            && it.r#type == FunctionType::FunctionDeclaration
            && let Some(id) = &it.id
        {
            self.definition(id.name.as_str());
            self.constraint(id.name.as_str(), BindingConstraintKind::Call);
        }

        self.function_depth += 1;
        walk_function(self, it, flags);
        self.function_depth -= 1;
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        self.function_depth += 1;
        walk_arrow_function_expression(self, it);
        self.function_depth -= 1;
    }

    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        if let Expression::Identifier(identifier) = &it.callee {
            self.constraint(identifier.name.as_str(), BindingConstraintKind::Call);
        }
        if let Some(kind) = iife_kind(&it.callee) {
            self.facts
                .push(AstFact::wrapper_region(self.module_id, kind));
        }
        match &it.callee {
            Expression::StaticMemberExpression(member) => {
                if let Some(object) = expression_identifier(&member.object) {
                    self.constraint(object, BindingConstraintKind::MemberRead);
                }
            }
            Expression::ComputedMemberExpression(member) => {
                if let Some(object) = expression_identifier(&member.object) {
                    self.constraint(object, BindingConstraintKind::MemberRead);
                }
            }
            _ => {}
        }

        if let Some(binding) = it.arguments.first().and_then(enum_initializer_binding) {
            self.constraint(binding, BindingConstraintKind::EnumInitializer);
            self.facts.push(AstFact {
                module_id: self.module_id,
                binding: Some(BindingName::new(binding)),
                kind: AstFactKind::WrapperRegion(AstWrapperKind::EnumIife),
            });
        }

        walk_call_expression(self, it);
    }

    fn visit_new_expression(&mut self, it: &NewExpression<'a>) {
        if let Some(binding) = expression_identifier(&it.callee) {
            self.constraint(binding, BindingConstraintKind::Construct);
        }
        walk_new_expression(self, it);
    }

    fn visit_static_member_expression(&mut self, it: &StaticMemberExpression<'a>) {
        if let Some(binding) = expression_identifier(&it.object) {
            self.constraint(binding, BindingConstraintKind::MemberRead);
        }
        walk_static_member_expression(self, it);
    }

    fn visit_computed_member_expression(&mut self, it: &ComputedMemberExpression<'a>) {
        if let Some(binding) = expression_identifier(&it.object) {
            self.constraint(binding, BindingConstraintKind::MemberRead);
        }
        walk_computed_member_expression(self, it);
    }
}

impl AstFactVisitor {
    fn definition(&mut self, binding: &str) {
        self.facts
            .push(AstFact::definition(self.module_id, binding));
    }

    fn read(&mut self, binding: &str) {
        self.facts.push(AstFact::read(self.module_id, binding));
    }

    fn constraint(&mut self, binding: &str, kind: BindingConstraintKind) {
        self.facts
            .push(AstFact::constraint(self.module_id, binding, kind));
    }
}

fn binding_pattern_name<'a>(pattern: &'a BindingPattern<'a>) -> Option<&'a str> {
    match &pattern.kind {
        BindingPatternKind::BindingIdentifier(identifier) => Some(identifier.name.as_str()),
        BindingPatternKind::AssignmentPattern(pattern) => binding_pattern_name(&pattern.left),
        BindingPatternKind::ObjectPattern(_) | BindingPatternKind::ArrayPattern(_) => None,
    }
}

fn expression_identifier<'a>(expression: &'a Expression<'a>) -> Option<&'a str> {
    match expression {
        Expression::Identifier(identifier) => Some(identifier.name.as_str()),
        Expression::StaticMemberExpression(member) => expression_identifier(&member.object),
        Expression::ComputedMemberExpression(member) => expression_identifier(&member.object),
        Expression::ParenthesizedExpression(parenthesized) => {
            expression_identifier(&parenthesized.expression)
        }
        _ => None,
    }
}

fn enum_initializer_binding<'a>(argument: &'a Argument<'a>) -> Option<&'a str> {
    match argument {
        Argument::Identifier(identifier) => Some(identifier.name.as_str()),
        Argument::LogicalExpression(logical) if logical.operator == LogicalOperator::Or => {
            let left = expression_identifier(&logical.left)?;
            let right = assignment_expression_target(&logical.right)?;
            (left == right).then_some(left)
        }
        Argument::AssignmentExpression(assignment) => {
            assignment_target_identifier(&assignment.left)
        }
        Argument::ParenthesizedExpression(parenthesized) => {
            expression_enum_initializer_binding(&parenthesized.expression)
        }
        _ => None,
    }
}

fn expression_enum_initializer_binding<'a>(expression: &'a Expression<'a>) -> Option<&'a str> {
    match expression {
        Expression::Identifier(identifier) => Some(identifier.name.as_str()),
        Expression::LogicalExpression(logical) if logical.operator == LogicalOperator::Or => {
            let left = expression_identifier(&logical.left)?;
            let right = assignment_expression_target(&logical.right)?;
            (left == right).then_some(left)
        }
        Expression::AssignmentExpression(assignment) => {
            assignment_target_identifier(&assignment.left)
        }
        Expression::ParenthesizedExpression(parenthesized) => {
            expression_enum_initializer_binding(&parenthesized.expression)
        }
        _ => None,
    }
}

fn assignment_expression_target<'a>(expression: &'a Expression<'a>) -> Option<&'a str> {
    match expression {
        Expression::AssignmentExpression(assignment)
            if assignment.operator == AssignmentOperator::Assign =>
        {
            assignment_target_identifier(&assignment.left)
        }
        Expression::ParenthesizedExpression(parenthesized) => {
            assignment_expression_target(&parenthesized.expression)
        }
        _ => None,
    }
}

fn assignment_target_identifier<'a>(target: &'a AssignmentTarget<'a>) -> Option<&'a str> {
    match target {
        AssignmentTarget::AssignmentTargetIdentifier(identifier) => Some(identifier.name.as_str()),
        AssignmentTarget::StaticMemberExpression(member) => expression_identifier(&member.object),
        AssignmentTarget::ComputedMemberExpression(member) => expression_identifier(&member.object),
        AssignmentTarget::TSAsExpression(_)
        | AssignmentTarget::TSSatisfiesExpression(_)
        | AssignmentTarget::TSNonNullExpression(_)
        | AssignmentTarget::TSTypeAssertion(_)
        | AssignmentTarget::TSInstantiationExpression(_)
        | AssignmentTarget::PrivateFieldExpression(_)
        | AssignmentTarget::ArrayAssignmentTarget(_)
        | AssignmentTarget::ObjectAssignmentTarget(_) => None,
    }
}

fn iife_kind(expression: &Expression<'_>) -> Option<AstWrapperKind> {
    match expression {
        Expression::FunctionExpression(_) => Some(AstWrapperKind::FunctionIife),
        Expression::ArrowFunctionExpression(_) => Some(AstWrapperKind::ArrowIife),
        Expression::ParenthesizedExpression(parenthesized) => iife_kind(&parenthesized.expression),
        _ => None,
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImportExportGraph {
    module_imports: BTreeMap<ModuleId, BTreeSet<ModuleId>>,
    package_imports: BTreeMap<ModuleId, BTreeSet<String>>,
    exports: BTreeMap<ModuleId, BTreeSet<BindingName>>,
}

impl ImportExportGraph {
    pub fn record_module_import(&mut self, from_module_id: ModuleId, target_module_id: ModuleId) {
        self.module_imports
            .entry(from_module_id)
            .or_default()
            .insert(target_module_id);
    }

    pub fn record_package_import(
        &mut self,
        from_module_id: ModuleId,
        specifier: impl Into<String>,
    ) {
        self.package_imports
            .entry(from_module_id)
            .or_default()
            .insert(specifier.into());
    }

    pub fn record_export(&mut self, module_id: ModuleId, binding: impl Into<String>) {
        self.exports
            .entry(module_id)
            .or_default()
            .insert(BindingName::new(binding));
    }

    #[must_use]
    pub fn package_imports_for(&self, module_id: ModuleId) -> Vec<&str> {
        self.package_imports
            .get(&module_id)
            .map(|imports| imports.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use reverts_input::{
        InputBundle, InputRows, ModuleInput, ProjectInput, SourceFileInput, SymbolInput,
    };
    use reverts_ir::{BindingConstraintKind, ModuleId};

    use super::{AstFactKind, AstWrapperKind, RevertsGraph};

    #[test]
    fn input_symbols_become_graph_definitions() {
        let mut rows = InputRows::new(ProjectInput {
            id: 1,
            name: "fixture".to_string(),
        });
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "m1", "src/index.ts"));
        rows.symbols.push(SymbolInput {
            module_id: ModuleId(1),
            name: "main".to_string(),
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let graph = RevertsGraph::from_input(&input);

        assert_eq!(graph.definitions_for(ModuleId(1))[0].as_str(), "main");
    }

    #[test]
    fn unresolved_reads_remain_visible_in_def_use_graph() {
        let mut rows = InputRows::new(ProjectInput {
            id: 1,
            name: "fixture".to_string(),
        });
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "m1", "src/index.ts"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let mut graph = RevertsGraph::from_input(&input);

        graph.record_read(ModuleId(1), "missing");

        assert_eq!(graph.def_use().unresolved_reads()[0].1.as_str(), "missing");
    }

    #[test]
    fn ast_fact_extractor_projects_enum_iife_into_shape_constraint() {
        let source = r#"
            var NativeModuleType;
            (function (NativeModuleType) {
                NativeModuleType[NativeModuleType["Vm"] = 0] = "Vm";
            })(NativeModuleType || (NativeModuleType = {}));
        "#;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some(source.to_string()),
        ));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "m1", "src/module.ts").with_source_file(1));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let graph = RevertsGraph::from_input(&input);

        assert!(graph.ast_errors().is_empty());
        assert!(graph.def_use().constraints().iter().any(|constraint| {
            constraint.binding.as_str() == "NativeModuleType"
                && constraint.kind == BindingConstraintKind::EnumInitializer
        }));
        assert!(graph.ast_facts().iter().any(|fact| {
            fact.binding
                .as_ref()
                .is_some_and(|binding| binding.as_str() == "NativeModuleType")
                && fact.kind == AstFactKind::WrapperRegion(AstWrapperKind::EnumIife)
        }));
    }

    #[test]
    fn ast_fact_extractor_projects_calls_constructs_members_and_classes() {
        let source = r#"
            class Service {}
            const ns = {};
            factory();
            new Service();
            ns.value;
        "#;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some(source.to_string()),
        ));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "m1", "src/module.ts").with_source_file(1));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let graph = RevertsGraph::from_input(&input);

        assert!(graph.ast_errors().is_empty());
        assert!(graph.def_use().constraints().iter().any(|constraint| {
            constraint.binding.as_str() == "Service"
                && constraint.kind == BindingConstraintKind::ClassDeclaration
        }));
        assert!(graph.def_use().constraints().iter().any(|constraint| {
            constraint.binding.as_str() == "factory"
                && constraint.kind == BindingConstraintKind::Call
        }));
        assert!(graph.def_use().constraints().iter().any(|constraint| {
            constraint.binding.as_str() == "Service"
                && constraint.kind == BindingConstraintKind::Construct
        }));
        assert!(graph.def_use().constraints().iter().any(|constraint| {
            constraint.binding.as_str() == "ns"
                && constraint.kind == BindingConstraintKind::MemberRead
        }));
    }
}
