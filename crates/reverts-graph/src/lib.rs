use std::collections::{BTreeMap, BTreeSet};

use oxc_allocator::Allocator;
use oxc_ast::{
    Visit,
    ast::{
        Argument, ArrowFunctionExpression, AssignmentExpression, AssignmentTarget, BindingPattern,
        BindingPatternKind, CallExpression, Class, ComputedMemberExpression, Declaration,
        ExportAllDeclaration, ExportDefaultDeclaration, ExportDefaultDeclarationKind,
        ExportNamedDeclaration, Expression, Function, FunctionType, ImportDeclaration,
        ImportDeclarationSpecifier, ImportExpression, ModuleExportName, NewExpression, Program,
        SimpleAssignmentTarget, Statement, StaticMemberExpression, UpdateExpression,
        VariableDeclaration, VariableDeclarator,
    },
    visit::walk::{
        walk_arrow_function_expression, walk_call_expression, walk_class,
        walk_computed_member_expression, walk_export_all_declaration,
        walk_export_default_declaration, walk_export_named_declaration, walk_function,
        walk_import_expression, walk_new_expression, walk_static_member_expression,
        walk_variable_declarator,
    },
};
use oxc_parser::{ParseOptions, Parser};
use oxc_span::GetSpan;
use oxc_syntax::{
    operator::{AssignmentOperator, LogicalOperator},
    scope::ScopeFlags,
};
use reverts_input::{InputBundle, ModuleDependencyTarget, ModuleInput};
use reverts_ir::{
    BindingConstraint, BindingConstraintKind, BindingName, ControlFlowEdgeKind, ControlFlowGraph,
    ControlFlowNodeKind, DefUseGraph, FlowNodeId, ModuleId, ModuleKind, split_bare_specifier,
};
use reverts_js::{JsError, ParseError, ParseGoal, parse_error_message, source_type_candidates};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevertsGraph {
    modules: BTreeMap<ModuleId, ModuleInput>,
    definitions: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    def_use: DefUseGraph,
    control_flow: ControlFlowGraph,
    import_export: ImportExportGraph,
    runtime_preludes: BTreeMap<u32, RuntimePrelude>,
    runtime_imports: BTreeMap<ModuleId, BTreeSet<RuntimePreludeImport>>,
    ast_facts: Vec<AstFact>,
    ast_errors: Vec<AstFactError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePrelude {
    pub source_file_id: u32,
    pub source_file_path: String,
    pub source: String,
    pub bindings: BTreeMap<BindingName, RuntimePreludeBindingKind>,
    pub snippets: BTreeMap<BindingName, RuntimePreludeSnippet>,
    pub entrypoint: Option<RuntimeEntrypoint>,
}

impl RuntimePrelude {
    #[must_use]
    pub fn defines(&self, binding: &BindingName) -> bool {
        self.bindings.contains_key(binding)
    }

    #[must_use]
    pub fn binding_kind(&self, binding: &BindingName) -> Option<RuntimePreludeBindingKind> {
        self.bindings.get(binding).copied()
    }

    #[must_use]
    pub fn source_for_bindings<'a>(
        &self,
        bindings: impl Iterator<Item = &'a BindingName>,
    ) -> String {
        let mut needed = bindings.cloned().collect::<BTreeSet<_>>();
        let mut visited = BTreeSet::<BindingName>::new();
        let mut snippets = BTreeMap::<u32, String>::new();

        while let Some(binding) = needed
            .iter()
            .find(|binding| !visited.contains(*binding))
            .cloned()
        {
            visited.insert(binding.clone());
            let Some(snippet) = self.snippets.get(&binding) else {
                continue;
            };
            snippets
                .entry(snippet.byte_start)
                .or_insert_with(|| snippet.source.clone());
            for identifier in identifiers_in_source(snippet.source.as_str()) {
                let candidate = BindingName::new(identifier);
                if self.bindings.contains_key(&candidate) && !visited.contains(&candidate) {
                    needed.insert(candidate);
                }
            }
        }

        snippets.into_values().collect::<Vec<_>>().join("\n")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePreludeSnippet {
    pub source: String,
    pub byte_start: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeEntrypoint {
    pub source_file_id: u32,
    pub callee: BindingName,
    pub statement_source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuntimePreludeBindingKind {
    CommonJsWrapper,
    LazyInitializer,
    SourceBacked,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RuntimePreludeImport {
    pub source_file_id: u32,
    pub binding: BindingName,
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
    pub fn write(module_id: ModuleId, binding: impl Into<String>) -> Self {
        Self {
            module_id,
            binding: Some(BindingName::new(binding)),
            kind: AstFactKind::Write,
        }
    }

    #[must_use]
    pub fn import(module_id: ModuleId, binding: impl Into<String>) -> Self {
        Self {
            module_id,
            binding: Some(BindingName::new(binding)),
            kind: AstFactKind::Import,
        }
    }

    #[must_use]
    pub fn package_import(module_id: ModuleId, specifier: impl Into<String>) -> Self {
        Self {
            module_id,
            binding: Some(BindingName::new(specifier)),
            kind: AstFactKind::PackageImport,
        }
    }

    #[must_use]
    pub fn export(module_id: ModuleId, binding: impl Into<String>) -> Self {
        Self {
            module_id,
            binding: Some(BindingName::new(binding)),
            kind: AstFactKind::Export,
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
    Write,
    Import,
    PackageImport,
    Export,
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

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AstExtraction {
    pub facts: Vec<AstFact>,
    pub control_flow: ControlFlowGraph,
}

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
        let source_backed_modules = input
            .modules
            .iter()
            .filter(|module| input.module_source_slice(module.id).is_some())
            .map(|module| module.id)
            .collect::<BTreeSet<_>>();
        for symbol in &input.symbols {
            if source_backed_modules.contains(&symbol.module_id) {
                continue;
            }
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
        let mut control_flow = ControlFlowGraph::default();
        let runtime_preludes = extract_runtime_preludes(input);
        for module in &input.modules {
            if module.kind == ModuleKind::Package {
                continue;
            }
            let Some(source) = input.module_source_slice(module.id) else {
                continue;
            };
            match AstFactExtractor.extract(module, source.source_file_path, source.source) {
                Ok(extraction) => {
                    control_flow.extend(extraction.control_flow);
                    for fact in extraction.facts {
                        apply_ast_fact(&mut definitions, &mut def_use, &mut import_export, &fact);
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
        let runtime_imports =
            resolve_runtime_prelude_imports(&modules, &runtime_preludes, &mut def_use);

        Self {
            modules,
            definitions,
            def_use,
            control_flow,
            import_export,
            runtime_preludes,
            runtime_imports,
            ast_facts,
            ast_errors,
        }
    }

    pub fn record_read(&mut self, module_id: ModuleId, binding: impl Into<String>) {
        self.def_use.read(module_id, binding);
    }

    pub fn record_write(&mut self, module_id: ModuleId, binding: impl Into<String>) {
        self.def_use.write(module_id, binding);
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
    pub fn control_flow(&self) -> &ControlFlowGraph {
        &self.control_flow
    }

    #[must_use]
    pub fn import_export(&self) -> &ImportExportGraph {
        &self.import_export
    }

    #[must_use]
    pub fn runtime_prelude(&self, source_file_id: u32) -> Option<&RuntimePrelude> {
        self.runtime_preludes.get(&source_file_id)
    }

    #[must_use]
    pub fn runtime_preludes(&self) -> &BTreeMap<u32, RuntimePrelude> {
        &self.runtime_preludes
    }

    #[must_use]
    pub fn runtime_imports_for(&self, module_id: ModuleId) -> Vec<RuntimePreludeImport> {
        self.runtime_imports
            .get(&module_id)
            .map(|imports| imports.iter().cloned().collect())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn ast_facts(&self) -> &[AstFact] {
        &self.ast_facts
    }

    #[must_use]
    pub fn ast_definitions_for(&self, module_id: ModuleId) -> BTreeSet<BindingName> {
        self.ast_bindings_for(module_id, |kind| *kind == AstFactKind::Definition)
    }

    #[must_use]
    pub fn ast_imports_for(&self, module_id: ModuleId) -> BTreeSet<BindingName> {
        self.ast_bindings_for(module_id, |kind| *kind == AstFactKind::Import)
    }

    fn ast_bindings_for(
        &self,
        module_id: ModuleId,
        predicate: impl Fn(&AstFactKind) -> bool,
    ) -> BTreeSet<BindingName> {
        self.ast_facts
            .iter()
            .filter(|fact| fact.module_id == module_id && predicate(&fact.kind))
            .filter_map(|fact| fact.binding.clone())
            .collect()
    }

    #[must_use]
    pub fn ast_errors(&self) -> &[AstFactError] {
        &self.ast_errors
    }
}

fn apply_ast_fact(
    definitions: &mut BTreeMap<ModuleId, BTreeSet<BindingName>>,
    def_use: &mut DefUseGraph,
    import_export: &mut ImportExportGraph,
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
        AstFactKind::Write => {
            if let Some(binding) = &fact.binding {
                def_use.write(fact.module_id, binding.as_str());
            }
        }
        AstFactKind::Import => {
            if let Some(binding) = &fact.binding {
                def_use.import(fact.module_id, binding.as_str());
            }
        }
        AstFactKind::PackageImport => {
            if let Some(specifier) = &fact.binding {
                import_export.record_package_import(fact.module_id, specifier.as_str());
            }
        }
        AstFactKind::Export => {
            if let Some(binding) = &fact.binding {
                import_export.record_export(fact.module_id, binding.as_str());
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

fn extract_runtime_preludes(input: &InputBundle) -> BTreeMap<u32, RuntimePrelude> {
    let mut preludes = BTreeMap::new();
    for source_file in &input.source_files {
        let Some(source) = source_file.source.as_deref() else {
            continue;
        };
        let module_spans = runtime_scope_module_spans(input, source_file.id);
        if module_spans.is_empty() {
            continue;
        }
        if let Some(prelude) = parse_runtime_prelude(
            source_file.id,
            source_file.path.as_str(),
            source,
            &module_spans,
        ) {
            preludes.insert(source_file.id, prelude);
        }
    }
    preludes
}

fn runtime_scope_module_spans(input: &InputBundle, source_file_id: u32) -> Vec<(u32, u32)> {
    let mut spans = input
        .modules
        .iter()
        .filter(|module| module.source_file_id == Some(source_file_id))
        .filter_map(|module| module.source_span)
        .map(|span| (span.byte_start, span.byte_end))
        .collect::<Vec<_>>();
    spans.sort_unstable();
    spans
}

fn parse_runtime_prelude(
    source_file_id: u32,
    source_file_path: &str,
    source: &str,
    module_spans: &[(u32, u32)],
) -> Option<RuntimePrelude> {
    let allocator = Allocator::default();
    for source_type in source_type_candidates(
        Some(std::path::Path::new(source_file_path)),
        ParseGoal::TypeScript,
    ) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if parsed.errors.is_empty() && !parsed.panicked {
            let (bindings, snippets, source, entrypoint) = collect_runtime_prelude_declarations(
                source_file_id,
                &parsed.program,
                source,
                module_spans,
            );
            if bindings.is_empty() {
                return None;
            }
            return Some(RuntimePrelude {
                source_file_id,
                source_file_path: source_file_path.to_string(),
                source,
                bindings,
                snippets,
                entrypoint,
            });
        }
    }

    None
}

fn collect_runtime_prelude_declarations(
    source_file_id: u32,
    program: &Program<'_>,
    source: &str,
    module_spans: &[(u32, u32)],
) -> (
    BTreeMap<BindingName, RuntimePreludeBindingKind>,
    BTreeMap<BindingName, RuntimePreludeSnippet>,
    String,
    Option<RuntimeEntrypoint>,
) {
    let mut bindings = BTreeMap::new();
    let mut snippets_by_binding = BTreeMap::new();
    let mut snippets = Vec::new();
    let mut entrypoint_candidate = None;
    for statement in &program.body {
        let span = statement.span();
        if !span_outside_module_spans(span.start, span.end, module_spans) {
            continue;
        }
        if let Some(candidate) =
            runtime_entrypoint_from_statement(source_file_id, statement, source)
        {
            entrypoint_candidate = Some(candidate);
        }
        let declarations = runtime_prelude_declarations_from_statement(statement, source);
        if declarations.is_empty() {
            continue;
        }
        for declaration in declarations {
            bindings.insert(declaration.binding.clone(), declaration.kind);
            snippets_by_binding.insert(
                declaration.binding,
                RuntimePreludeSnippet {
                    source: declaration.source.clone(),
                    byte_start: declaration.byte_start,
                },
            );
            snippets.push(declaration.source);
        }
    }
    let entrypoint =
        entrypoint_candidate.filter(|candidate| bindings.contains_key(&candidate.callee));
    (
        bindings,
        snippets_by_binding,
        snippets.join("\n"),
        entrypoint,
    )
}

fn runtime_entrypoint_from_statement(
    source_file_id: u32,
    statement: &Statement<'_>,
    source: &str,
) -> Option<RuntimeEntrypoint> {
    let Statement::ExpressionStatement(statement) = statement else {
        return None;
    };
    let Expression::CallExpression(call) = &statement.expression else {
        return None;
    };
    let Expression::Identifier(callee) = &call.callee else {
        return None;
    };
    let span = statement.span();
    let statement_source = source
        .get(span.start as usize..span.end as usize)?
        .to_string();
    Some(RuntimeEntrypoint {
        source_file_id,
        callee: BindingName::new(callee.name.as_str()),
        statement_source,
    })
}

fn span_outside_module_spans(start: u32, end: u32, module_spans: &[(u32, u32)]) -> bool {
    module_spans
        .iter()
        .all(|(module_start, module_end)| end <= *module_start || start >= *module_end)
}

struct RuntimePreludeDeclaration {
    binding: BindingName,
    kind: RuntimePreludeBindingKind,
    source: String,
    byte_start: u32,
}

fn runtime_prelude_declarations_from_statement(
    statement: &Statement<'_>,
    source: &str,
) -> Vec<RuntimePreludeDeclaration> {
    let mut declarations = Vec::new();
    match statement {
        Statement::VariableDeclaration(declaration) => {
            let keyword = variable_declaration_keyword(declaration, source);
            for declarator in &declaration.declarations {
                let kind = declarator
                    .init
                    .as_ref()
                    .map_or(RuntimePreludeBindingKind::SourceBacked, |init| {
                        runtime_binding_kind_from_initializer(init, source)
                    });
                for binding in binding_pattern_names(&declarator.id) {
                    declarations.push(RuntimePreludeDeclaration {
                        binding: BindingName::new(binding),
                        kind,
                        source: variable_declarator_snippet(keyword.as_str(), declarator, source),
                        byte_start: declarator.span.start,
                    });
                }
            }
        }
        Statement::FunctionDeclaration(function) => {
            if let Some(id) = &function.id {
                declarations.push(RuntimePreludeDeclaration {
                    binding: BindingName::new(id.name.as_str()),
                    kind: RuntimePreludeBindingKind::SourceBacked,
                    source: statement_snippet(statement, source),
                    byte_start: statement.span().start,
                });
            }
        }
        Statement::ClassDeclaration(class) => {
            if let Some(id) = &class.id {
                declarations.push(RuntimePreludeDeclaration {
                    binding: BindingName::new(id.name.as_str()),
                    kind: RuntimePreludeBindingKind::SourceBacked,
                    source: statement_snippet(statement, source),
                    byte_start: statement.span().start,
                });
            }
        }
        Statement::ImportDeclaration(declaration) => {
            let mut imported = BTreeSet::new();
            collect_import_module_bindings(declaration, &mut imported);
            for binding in imported {
                declarations.push(RuntimePreludeDeclaration {
                    binding: BindingName::new(binding),
                    kind: RuntimePreludeBindingKind::SourceBacked,
                    source: statement_snippet(statement, source),
                    byte_start: statement.span().start,
                });
            }
        }
        Statement::ExportNamedDeclaration(declaration) => {
            if let Some(declaration) = &declaration.declaration {
                for binding in declaration_binding_names(declaration) {
                    declarations.push(RuntimePreludeDeclaration {
                        binding: BindingName::new(binding),
                        kind: RuntimePreludeBindingKind::SourceBacked,
                        source: statement_snippet(statement, source),
                        byte_start: statement.span().start,
                    });
                }
            }
        }
        Statement::ExportDefaultDeclaration(declaration) => match &declaration.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(function) => {
                if let Some(id) = &function.id {
                    declarations.push(RuntimePreludeDeclaration {
                        binding: BindingName::new(id.name.as_str()),
                        kind: RuntimePreludeBindingKind::SourceBacked,
                        source: statement_snippet(statement, source),
                        byte_start: statement.span().start,
                    });
                }
            }
            ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                if let Some(id) = &class.id {
                    declarations.push(RuntimePreludeDeclaration {
                        binding: BindingName::new(id.name.as_str()),
                        kind: RuntimePreludeBindingKind::SourceBacked,
                        source: statement_snippet(statement, source),
                        byte_start: statement.span().start,
                    });
                }
            }
            _ => {}
        },
        _ => {}
    }
    declarations
}

fn variable_declaration_keyword(declaration: &VariableDeclaration<'_>, source: &str) -> String {
    let Some(first) = declaration.declarations.first() else {
        return "var".to_string();
    };
    source
        .get(declaration.span.start as usize..first.span.start as usize)
        .map(str::trim)
        .filter(|keyword| matches!(*keyword, "var" | "let" | "const"))
        .unwrap_or("var")
        .to_string()
}

fn variable_declarator_snippet(
    keyword: &str,
    declarator: &VariableDeclarator<'_>,
    source: &str,
) -> String {
    source
        .get(declarator.span.start as usize..declarator.span.end as usize)
        .map(str::trim)
        .map(|declarator| format!("{keyword} {declarator};"))
        .unwrap_or_else(|| format!("{keyword};"))
}

fn statement_snippet(statement: &Statement<'_>, source: &str) -> String {
    let span = statement.span();
    source
        .get(span.start as usize..span.end as usize)
        .unwrap_or_default()
        .to_string()
}

fn runtime_binding_kind_from_initializer(
    init: &Expression<'_>,
    source: &str,
) -> RuntimePreludeBindingKind {
    let span = init.span();
    let Some(snippet) = source.get(span.start as usize..span.end as usize) else {
        return RuntimePreludeBindingKind::SourceBacked;
    };
    let compact = compact_source(snippet);
    if looks_like_commonjs_wrapper(&compact) {
        RuntimePreludeBindingKind::CommonJsWrapper
    } else if looks_like_lazy_initializer(&compact) {
        RuntimePreludeBindingKind::LazyInitializer
    } else {
        RuntimePreludeBindingKind::SourceBacked
    }
}

fn compact_source(source: &str) -> String {
    source
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn identifiers_in_source(source: &str) -> BTreeSet<String> {
    let mut identifiers = BTreeSet::new();
    let bytes = source.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if matches!(byte, b'\'' | b'"' | b'`') {
            index = skip_quoted_source(bytes, index, byte);
            continue;
        }
        if byte == b'/' && bytes.get(index + 1) == Some(&b'/') {
            index = skip_line_comment(bytes, index + 2);
            continue;
        }
        if byte == b'/' && bytes.get(index + 1) == Some(&b'*') {
            index = skip_block_comment(bytes, index + 2);
            continue;
        }
        if is_identifier_start(byte) {
            let start = index;
            index += 1;
            while index < bytes.len() && is_identifier_continue(bytes[index]) {
                index += 1;
            }
            let identifier = &source[start..index];
            if !is_js_keyword(identifier) {
                identifiers.insert(identifier.to_string());
            }
            continue;
        }
        index += 1;
    }
    identifiers
}

fn skip_quoted_source(bytes: &[u8], start: usize, quote: u8) -> usize {
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index += 2;
            continue;
        }
        if bytes[index] == quote {
            return index + 1;
        }
        index += 1;
    }
    bytes.len()
}

fn skip_line_comment(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && bytes[index] != b'\n' {
        index += 1;
    }
    index
}

fn skip_block_comment(bytes: &[u8], mut index: usize) -> usize {
    while index + 1 < bytes.len() {
        if bytes[index] == b'*' && bytes[index + 1] == b'/' {
            return index + 2;
        }
        index += 1;
    }
    bytes.len()
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$')
}

fn is_identifier_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit()
}

fn is_js_keyword(value: &str) -> bool {
    matches!(
        value,
        "as" | "async"
            | "await"
            | "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "default"
            | "do"
            | "else"
            | "export"
            | "extends"
            | "false"
            | "finally"
            | "for"
            | "from"
            | "function"
            | "if"
            | "import"
            | "in"
            | "let"
            | "new"
            | "null"
            | "return"
            | "super"
            | "switch"
            | "this"
            | "throw"
            | "true"
            | "try"
            | "typeof"
            | "undefined"
            | "var"
            | "void"
            | "while"
            | "with"
            | "yield"
    )
}

fn looks_like_commonjs_wrapper(compact: &str) -> bool {
    compact.contains("=>()=>") && compact.contains("{exports:{}}") && compact.contains(".exports")
}

fn looks_like_lazy_initializer(compact: &str) -> bool {
    compact.contains("=>()=>") && compact.contains("&&") && compact.contains("=0")
}

fn resolve_runtime_prelude_imports(
    modules: &BTreeMap<ModuleId, ModuleInput>,
    runtime_preludes: &BTreeMap<u32, RuntimePrelude>,
    def_use: &mut DefUseGraph,
) -> BTreeMap<ModuleId, BTreeSet<RuntimePreludeImport>> {
    let unresolved_writes = def_use
        .unresolved_writes()
        .into_iter()
        .collect::<BTreeSet<_>>();
    let mut runtime_imports = BTreeMap::<ModuleId, BTreeSet<RuntimePreludeImport>>::new();

    for (module_id, binding) in def_use.unresolved_reads() {
        if unresolved_writes.contains(&(module_id, binding.clone())) {
            continue;
        }
        let Some(module) = modules.get(&module_id) else {
            continue;
        };
        let Some(source_file_id) = module.source_file_id else {
            continue;
        };
        let Some(prelude) = runtime_preludes.get(&source_file_id) else {
            continue;
        };
        if !prelude.defines(&binding) {
            continue;
        }

        def_use.import(module_id, binding.as_str());
        runtime_imports
            .entry(module_id)
            .or_default()
            .insert(RuntimePreludeImport {
                source_file_id,
                binding,
            });
    }

    runtime_imports
}

impl AstFactExtractor {
    pub fn extract(
        self,
        module: &ModuleInput,
        source_path: &str,
        source: &str,
    ) -> Result<AstExtraction, String> {
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
                    module_scope_bindings: collect_module_scope_bindings(&parsed.program),
                };
                visitor.visit_program(&parsed.program);
                return Ok(AstExtraction {
                    facts: visitor.facts,
                    control_flow: extract_control_flow(module.id, &parsed.program),
                });
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
}

fn extract_control_flow(module_id: ModuleId, program: &Program<'_>) -> ControlFlowGraph {
    let mut graph = ControlFlowGraph::default();
    let entry = graph.add_node(module_id, ControlFlowNodeKind::Entry);
    let mut previous = entry;

    for (index, statement) in program.body.iter().enumerate() {
        previous = append_statement_flow(module_id, &mut graph, previous, statement, index == 0);
    }

    let exit = graph.add_node(module_id, ControlFlowNodeKind::Exit);
    let edge_kind = if previous == entry {
        ControlFlowEdgeKind::Entry
    } else {
        ControlFlowEdgeKind::Sequential
    };
    graph.add_edge(module_id, previous, exit, edge_kind);
    graph
}

fn append_statement_flow(
    module_id: ModuleId,
    graph: &mut ControlFlowGraph,
    previous: FlowNodeId,
    statement: &Statement<'_>,
    is_first_statement: bool,
) -> FlowNodeId {
    let kind = control_flow_node_kind(statement);
    let current = graph.add_node(module_id, kind);
    let edge_kind = if is_first_statement {
        ControlFlowEdgeKind::Entry
    } else if matches!(
        kind,
        ControlFlowNodeKind::Return | ControlFlowNodeKind::Throw
    ) {
        ControlFlowEdgeKind::Termination
    } else {
        ControlFlowEdgeKind::Sequential
    };
    graph.add_edge(module_id, previous, current, edge_kind);

    match statement {
        Statement::IfStatement(statement) => {
            add_conditional_statement(module_id, graph, current, &statement.consequent);
            if let Some(alternate) = &statement.alternate {
                add_conditional_statement(module_id, graph, current, alternate);
            }
        }
        Statement::ForStatement(statement) => {
            add_loop_body(module_id, graph, current, &statement.body);
        }
        Statement::ForInStatement(statement) => {
            add_loop_body(module_id, graph, current, &statement.body);
        }
        Statement::ForOfStatement(statement) => {
            add_loop_body(module_id, graph, current, &statement.body);
        }
        Statement::WhileStatement(statement) => {
            add_loop_body(module_id, graph, current, &statement.body);
        }
        Statement::DoWhileStatement(statement) => {
            add_loop_body(module_id, graph, current, &statement.body);
        }
        _ => {}
    }

    if kind == ControlFlowNodeKind::Loop {
        graph.add_edge(module_id, current, current, ControlFlowEdgeKind::LoopBack);
    }

    current
}

fn add_conditional_statement(
    module_id: ModuleId,
    graph: &mut ControlFlowGraph,
    branch: FlowNodeId,
    statement: &Statement<'_>,
) {
    let target = graph.add_node(module_id, control_flow_node_kind(statement));
    graph.add_edge(module_id, branch, target, ControlFlowEdgeKind::Conditional);
}

fn add_loop_body(
    module_id: ModuleId,
    graph: &mut ControlFlowGraph,
    loop_node: FlowNodeId,
    statement: &Statement<'_>,
) {
    let body = graph.add_node(module_id, control_flow_node_kind(statement));
    graph.add_edge(module_id, loop_node, body, ControlFlowEdgeKind::Conditional);
}

fn control_flow_node_kind(statement: &Statement<'_>) -> ControlFlowNodeKind {
    match statement {
        Statement::IfStatement(_) | Statement::SwitchStatement(_) => ControlFlowNodeKind::Branch,
        Statement::ForStatement(_)
        | Statement::ForInStatement(_)
        | Statement::ForOfStatement(_)
        | Statement::WhileStatement(_)
        | Statement::DoWhileStatement(_) => ControlFlowNodeKind::Loop,
        Statement::ReturnStatement(_) => ControlFlowNodeKind::Return,
        Statement::ThrowStatement(_) => ControlFlowNodeKind::Throw,
        _ => ControlFlowNodeKind::Statement,
    }
}

fn parse_options_for(source_type: oxc_span::SourceType) -> ParseOptions {
    ParseOptions {
        allow_return_outside_function: source_type.is_script(),
        ..Default::default()
    }
}

struct AstFactVisitor {
    module_id: ModuleId,
    facts: Vec<AstFact>,
    function_depth: usize,
    module_scope_bindings: BTreeSet<String>,
}

impl<'a> Visit<'a> for AstFactVisitor {
    fn visit_identifier_reference(&mut self, it: &oxc_ast::ast::IdentifierReference<'a>) {
        self.read(it.name.as_str());
    }

    fn visit_import_declaration(&mut self, it: &ImportDeclaration<'a>) {
        let specifier = it.source.value.as_str();
        if split_bare_specifier(specifier).is_some() {
            self.package_import(specifier);
        }

        if let Some(specifiers) = &it.specifiers {
            for specifier in specifiers {
                match specifier {
                    ImportDeclarationSpecifier::ImportSpecifier(specifier) => {
                        self.import(specifier.local.name.as_str());
                    }
                    ImportDeclarationSpecifier::ImportDefaultSpecifier(specifier) => {
                        self.import(specifier.local.name.as_str());
                    }
                    ImportDeclarationSpecifier::ImportNamespaceSpecifier(specifier) => {
                        self.import(specifier.local.name.as_str());
                    }
                }
            }
        }
    }

    fn visit_export_named_declaration(&mut self, it: &ExportNamedDeclaration<'a>) {
        if let Some(source) = &it.source {
            let specifier = source.value.as_str();
            if split_bare_specifier(specifier).is_some() {
                self.package_import(specifier);
            }
        }

        if let Some(declaration) = &it.declaration {
            for binding in declaration_binding_names(declaration) {
                self.export(binding);
            }
        }

        for specifier in &it.specifiers {
            if it.source.is_none() {
                if let Some(local) = module_export_name(&specifier.local) {
                    self.export(local);
                }
            } else if let Some(exported) = module_export_name(&specifier.exported) {
                self.export(exported);
            }
        }

        walk_export_named_declaration(self, it);
    }

    fn visit_export_default_declaration(&mut self, it: &ExportDefaultDeclaration<'a>) {
        self.export("default");
        walk_export_default_declaration(self, it);
    }

    fn visit_export_all_declaration(&mut self, it: &ExportAllDeclaration<'a>) {
        let specifier = it.source.value.as_str();
        if split_bare_specifier(specifier).is_some() {
            self.package_import(specifier);
        }
        if let Some(exported) = &it.exported
            && let Some(binding) = module_export_name(exported)
        {
            self.export(binding);
        }
        walk_export_all_declaration(self, it);
    }

    fn visit_variable_declarator(&mut self, it: &VariableDeclarator<'a>) {
        if self.function_depth == 0 {
            let bindings = binding_pattern_names(&it.id);
            for binding in &bindings {
                self.definition(binding);
            }
            if let Some(init) = &it.init
                && bindings.len() == 1
                && let Some(kind) = initializer_constraint_kind(init)
            {
                self.constraint(bindings[0], kind);
            }
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

    fn visit_assignment_expression(&mut self, it: &AssignmentExpression<'a>) {
        if let Some(binding) = commonjs_export_binding(&it.left, &it.right) {
            self.export(binding);
        }
        if let Some(binding) = assignment_target_identifier(&it.left) {
            self.write(binding);
            if !it.operator.is_assign() {
                self.read(binding);
            }
            if assignment_target_is_member(&it.left) {
                self.constraint(binding, BindingConstraintKind::MemberWrite);
            }
        }

        self.visit_expression(&it.right);
    }

    fn visit_update_expression(&mut self, it: &UpdateExpression<'a>) {
        if let Some(binding) = simple_assignment_target_identifier(&it.argument) {
            self.read(binding);
            self.write(binding);
            if simple_assignment_target_is_member(&it.argument) {
                self.constraint(binding, BindingConstraintKind::MemberWrite);
            }
        }
    }

    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        if let Expression::Identifier(identifier) = &it.callee {
            self.constraint(identifier.name.as_str(), BindingConstraintKind::Call);
        }

        if let Expression::Identifier(identifier) = &it.callee
            && identifier.name.as_str() == "require"
            && let Some(specifier) = it.arguments.first().and_then(argument_string_literal)
            && split_bare_specifier(specifier).is_some()
        {
            self.package_import(specifier);
        }

        if self.function_depth == 0
            && let Some(kind) = iife_kind(&it.callee)
        {
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

        if self.function_depth == 0
            && let Some(binding) = it.arguments.first().and_then(enum_initializer_binding)
        {
            self.constraint(binding, BindingConstraintKind::EnumInitializer);
            self.facts.push(AstFact {
                module_id: self.module_id,
                binding: Some(BindingName::new(binding)),
                kind: AstFactKind::WrapperRegion(AstWrapperKind::EnumIife),
            });
        }

        walk_call_expression(self, it);
    }

    fn visit_import_expression(&mut self, it: &ImportExpression<'a>) {
        if let Some(specifier) = expression_string_literal(&it.source)
            && split_bare_specifier(specifier).is_some()
        {
            self.package_import(specifier);
        }
        walk_import_expression(self, it);
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
        if self.function_depth == 0 {
            self.module_scope_bindings.insert(binding.to_string());
        }
        self.facts
            .push(AstFact::definition(self.module_id, binding));
    }

    fn read(&mut self, binding: &str) {
        if self.should_emit_binding_fact(binding) {
            self.facts.push(AstFact::read(self.module_id, binding));
        }
    }

    fn write(&mut self, binding: &str) {
        if self.should_emit_binding_fact(binding) {
            self.facts.push(AstFact::write(self.module_id, binding));
        }
    }

    fn import(&mut self, binding: &str) {
        self.module_scope_bindings.insert(binding.to_string());
        self.facts.push(AstFact::import(self.module_id, binding));
    }

    fn package_import(&mut self, specifier: &str) {
        self.facts
            .push(AstFact::package_import(self.module_id, specifier));
    }

    fn export(&mut self, binding: &str) {
        self.facts.push(AstFact::export(self.module_id, binding));
    }

    fn constraint(&mut self, binding: &str, kind: BindingConstraintKind) {
        if self.should_emit_binding_fact(binding) {
            self.facts
                .push(AstFact::constraint(self.module_id, binding, kind));
        }
    }

    fn should_emit_binding_fact(&self, binding: &str) -> bool {
        self.function_depth == 0 || self.module_scope_bindings.contains(binding)
    }
}

fn collect_module_scope_bindings(program: &Program<'_>) -> BTreeSet<String> {
    let mut bindings = BTreeSet::new();
    for statement in &program.body {
        collect_statement_module_bindings(statement, &mut bindings);
    }
    bindings
}

fn collect_statement_module_bindings(statement: &Statement<'_>, bindings: &mut BTreeSet<String>) {
    match statement {
        Statement::VariableDeclaration(declaration) => {
            for declarator in &declaration.declarations {
                for binding in binding_pattern_names(&declarator.id) {
                    bindings.insert(binding.to_string());
                }
            }
        }
        Statement::FunctionDeclaration(function) => {
            if let Some(id) = &function.id {
                bindings.insert(id.name.as_str().to_string());
            }
        }
        Statement::ClassDeclaration(class) => {
            if let Some(id) = &class.id {
                bindings.insert(id.name.as_str().to_string());
            }
        }
        Statement::ImportDeclaration(declaration) => {
            collect_import_module_bindings(declaration, bindings);
        }
        Statement::ExportNamedDeclaration(declaration) => {
            if let Some(declaration) = &declaration.declaration {
                for binding in declaration_binding_names(declaration) {
                    bindings.insert(binding.to_string());
                }
            }
        }
        Statement::ExportDefaultDeclaration(declaration) => match &declaration.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(function) => {
                if let Some(id) = &function.id {
                    bindings.insert(id.name.as_str().to_string());
                }
            }
            ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                if let Some(id) = &class.id {
                    bindings.insert(id.name.as_str().to_string());
                }
            }
            _ => {}
        },
        _ => {}
    }
}

fn collect_import_module_bindings(
    declaration: &ImportDeclaration<'_>,
    bindings: &mut BTreeSet<String>,
) {
    if let Some(specifiers) = &declaration.specifiers {
        for specifier in specifiers {
            match specifier {
                ImportDeclarationSpecifier::ImportSpecifier(specifier) => {
                    bindings.insert(specifier.local.name.as_str().to_string());
                }
                ImportDeclarationSpecifier::ImportDefaultSpecifier(specifier) => {
                    bindings.insert(specifier.local.name.as_str().to_string());
                }
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(specifier) => {
                    bindings.insert(specifier.local.name.as_str().to_string());
                }
            }
        }
    }
}

fn binding_pattern_names<'a>(pattern: &'a BindingPattern<'a>) -> Vec<&'a str> {
    let mut names = Vec::new();
    collect_binding_pattern_names(pattern, &mut names);
    names
}

fn collect_binding_pattern_names<'a>(pattern: &'a BindingPattern<'a>, names: &mut Vec<&'a str>) {
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

fn declaration_binding_names<'a>(declaration: &'a Declaration<'a>) -> Vec<&'a str> {
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

fn module_export_name<'a>(name: &'a ModuleExportName<'a>) -> Option<&'a str> {
    match name {
        ModuleExportName::IdentifierName(identifier) => Some(identifier.name.as_str()),
        ModuleExportName::IdentifierReference(identifier) => Some(identifier.name.as_str()),
        ModuleExportName::StringLiteral(literal) => Some(literal.value.as_str()),
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
        Expression::TSAsExpression(expression) => expression_identifier(&expression.expression),
        Expression::TSSatisfiesExpression(expression) => {
            expression_identifier(&expression.expression)
        }
        Expression::TSNonNullExpression(expression) => {
            expression_identifier(&expression.expression)
        }
        Expression::TSTypeAssertion(expression) => expression_identifier(&expression.expression),
        Expression::TSInstantiationExpression(expression) => {
            expression_identifier(&expression.expression)
        }
        _ => None,
    }
}

fn argument_string_literal<'a>(argument: &'a Argument<'a>) -> Option<&'a str> {
    match argument {
        Argument::StringLiteral(literal) => Some(literal.value.as_str()),
        Argument::ParenthesizedExpression(parenthesized) => {
            expression_string_literal(&parenthesized.expression)
        }
        _ => None,
    }
}

fn expression_string_literal<'a>(expression: &'a Expression<'a>) -> Option<&'a str> {
    match expression {
        Expression::StringLiteral(literal) => Some(literal.value.as_str()),
        Expression::ParenthesizedExpression(parenthesized) => {
            expression_string_literal(&parenthesized.expression)
        }
        _ => None,
    }
}

fn initializer_constraint_kind(expression: &Expression<'_>) -> Option<BindingConstraintKind> {
    match expression {
        Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_) => {
            Some(BindingConstraintKind::Call)
        }
        Expression::ClassExpression(_) => Some(BindingConstraintKind::ClassDeclaration),
        Expression::ObjectExpression(_) => Some(BindingConstraintKind::ObjectLiteralDeclaration),
        Expression::ParenthesizedExpression(parenthesized) => {
            initializer_constraint_kind(&parenthesized.expression)
        }
        _ => None,
    }
}

fn enum_initializer_binding<'a>(argument: &'a Argument<'a>) -> Option<&'a str> {
    match argument {
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
        AssignmentTarget::TSAsExpression(expression) => {
            expression_identifier(&expression.expression)
        }
        AssignmentTarget::TSSatisfiesExpression(expression) => {
            expression_identifier(&expression.expression)
        }
        AssignmentTarget::TSNonNullExpression(expression) => {
            expression_identifier(&expression.expression)
        }
        AssignmentTarget::TSTypeAssertion(expression) => {
            expression_identifier(&expression.expression)
        }
        AssignmentTarget::TSInstantiationExpression(expression) => {
            expression_identifier(&expression.expression)
        }
        AssignmentTarget::PrivateFieldExpression(_)
        | AssignmentTarget::ArrayAssignmentTarget(_)
        | AssignmentTarget::ObjectAssignmentTarget(_) => None,
    }
}

fn commonjs_export_binding<'a>(
    target: &'a AssignmentTarget<'a>,
    right: &'a Expression<'a>,
) -> Option<&'a str> {
    let AssignmentTarget::StaticMemberExpression(member) = target else {
        return None;
    };

    if expression_is_identifier(&member.object, "exports") {
        return Some(member.property.name.as_str());
    }

    if expression_is_static_member(&member.object, "module", "exports") {
        return Some(member.property.name.as_str());
    }

    if expression_is_identifier(&member.object, "module")
        && member.property.name.as_str() == "exports"
    {
        return expression_identifier(right);
    }

    None
}

fn expression_is_identifier(expression: &Expression<'_>, expected: &str) -> bool {
    matches!(expression, Expression::Identifier(identifier) if identifier.name.as_str() == expected)
}

fn expression_is_static_member(
    expression: &Expression<'_>,
    object_name: &str,
    property_name: &str,
) -> bool {
    matches!(
        expression,
        Expression::StaticMemberExpression(member)
            if expression_is_identifier(&member.object, object_name)
                && member.property.name.as_str() == property_name
    )
}

fn assignment_target_is_member(target: &AssignmentTarget<'_>) -> bool {
    matches!(
        target,
        AssignmentTarget::StaticMemberExpression(_)
            | AssignmentTarget::ComputedMemberExpression(_)
            | AssignmentTarget::PrivateFieldExpression(_)
    )
}

fn simple_assignment_target_identifier<'a>(
    target: &'a SimpleAssignmentTarget<'a>,
) -> Option<&'a str> {
    match target {
        SimpleAssignmentTarget::AssignmentTargetIdentifier(identifier) => {
            Some(identifier.name.as_str())
        }
        SimpleAssignmentTarget::StaticMemberExpression(member) => {
            expression_identifier(&member.object)
        }
        SimpleAssignmentTarget::ComputedMemberExpression(member) => {
            expression_identifier(&member.object)
        }
        SimpleAssignmentTarget::TSAsExpression(expression) => {
            expression_identifier(&expression.expression)
        }
        SimpleAssignmentTarget::TSSatisfiesExpression(expression) => {
            expression_identifier(&expression.expression)
        }
        SimpleAssignmentTarget::TSNonNullExpression(expression) => {
            expression_identifier(&expression.expression)
        }
        SimpleAssignmentTarget::TSTypeAssertion(expression) => {
            expression_identifier(&expression.expression)
        }
        SimpleAssignmentTarget::TSInstantiationExpression(expression) => {
            expression_identifier(&expression.expression)
        }
        SimpleAssignmentTarget::PrivateFieldExpression(_) => None,
    }
}

fn simple_assignment_target_is_member(target: &SimpleAssignmentTarget<'_>) -> bool {
    matches!(
        target,
        SimpleAssignmentTarget::StaticMemberExpression(_)
            | SimpleAssignmentTarget::ComputedMemberExpression(_)
            | SimpleAssignmentTarget::PrivateFieldExpression(_)
    )
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
    pub fn exports_for(&self, module_id: ModuleId) -> Vec<BindingName> {
        self.exports
            .get(&module_id)
            .map(|exports| exports.iter().cloned().collect())
            .unwrap_or_default()
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
        InputBundle, InputRows, ModuleInput, ProjectInput, SourceFileInput, SourceSpan, SymbolInput,
    };
    use reverts_ir::{
        BindingConstraintKind, BindingName, ControlFlowEdgeKind, ControlFlowNodeKind, ModuleId,
    };

    use super::{AstFactKind, AstWrapperKind, RevertsGraph, RuntimePreludeBindingKind};

    #[test]
    fn input_symbols_become_graph_definitions() {
        let mut rows = InputRows::new(ProjectInput {
            id: 1,
            name: "fixture".to_string(),
        });
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "m1", "src/index.ts"));
        rows.symbols.push(SymbolInput::new(ModuleId(1), "main"));
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
    fn ast_fact_extractor_does_not_treat_plain_call_arguments_as_enum_iifes() {
        let source = r#"
            function inherits(child, parent) {}
            function Child() {}
            function Parent() {}
            inherits(Child, Parent);
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
        assert!(!graph.def_use().constraints().iter().any(|constraint| {
            constraint.binding.as_str() == "Child"
                && constraint.kind == BindingConstraintKind::EnumInitializer
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

    #[test]
    fn ast_fact_extractor_projects_imports_exports_and_writes() {
        let source = r#"
            import value, { named as alias } from "pkg";
            let local = alias;
            local = value;
            local += 1;
            export { local };
        "#;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.mjs",
            Some(source.to_string()),
        ));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "m1", "src/module.ts").with_source_file(1));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let graph = RevertsGraph::from_input(&input);

        assert!(graph.ast_errors().is_empty());
        assert!(graph.def_use().unresolved_reads().is_empty());
        assert!(graph.def_use().unresolved_writes().is_empty());
        assert!(graph.ast_facts().iter().any(|fact| {
            fact.kind == AstFactKind::Import
                && fact
                    .binding
                    .as_ref()
                    .is_some_and(|binding| binding.as_str() == "alias")
        }));
        assert!(
            graph
                .import_export()
                .package_imports_for(ModuleId(1))
                .contains(&"pkg")
        );
        assert!(graph.ast_facts().iter().any(|fact| {
            fact.kind == AstFactKind::Write
                && fact
                    .binding
                    .as_ref()
                    .is_some_and(|binding| binding.as_str() == "local")
        }));
        assert!(
            graph
                .import_export()
                .exports_for(ModuleId(1))
                .iter()
                .any(|binding| binding.as_str() == "local")
        );
    }

    #[test]
    fn ast_fact_extractor_projects_control_flow_shape() {
        let source = r#"
            if (flag) {
                work();
            } else {
                fallback();
            }
            while (flag) {
                tick();
            }
            return;
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
        let flow = graph.control_flow();

        assert!(
            flow.nodes_for(ModuleId(1))
                .iter()
                .any(|node| { node.kind == ControlFlowNodeKind::Entry })
        );
        assert!(
            flow.nodes_for(ModuleId(1))
                .iter()
                .any(|node| { node.kind == ControlFlowNodeKind::Branch })
        );
        assert!(
            flow.nodes_for(ModuleId(1))
                .iter()
                .any(|node| { node.kind == ControlFlowNodeKind::Loop })
        );
        assert!(
            flow.nodes_for(ModuleId(1))
                .iter()
                .any(|node| { node.kind == ControlFlowNodeKind::Return })
        );
        assert!(
            flow.edges_for(ModuleId(1))
                .iter()
                .any(|edge| { edge.kind == ControlFlowEdgeKind::Conditional })
        );
        assert!(
            flow.edges_for(ModuleId(1))
                .iter()
                .any(|edge| { edge.kind == ControlFlowEdgeKind::LoopBack })
        );
    }

    #[test]
    fn ast_fact_extractor_projects_member_writes_into_shape_constraints() {
        let source = r#"
            const namespace = {};
            namespace.value = 1;
            namespace.count++;
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

        assert!(graph.def_use().constraints().iter().any(|constraint| {
            constraint.binding.as_str() == "namespace"
                && constraint.kind == BindingConstraintKind::MemberWrite
        }));
    }

    #[test]
    fn ast_fact_extractor_projects_top_level_initializer_shapes() {
        let source = r#"
            const callable = () => 42;
            const Constructable = class {};
            const bag = {};
            callable();
            new Constructable();
            bag.value;
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
            constraint.binding.as_str() == "callable"
                && constraint.kind == BindingConstraintKind::Call
        }));
        assert!(graph.def_use().constraints().iter().any(|constraint| {
            constraint.binding.as_str() == "Constructable"
                && constraint.kind == BindingConstraintKind::ClassDeclaration
        }));
        assert!(graph.def_use().constraints().iter().any(|constraint| {
            constraint.binding.as_str() == "bag"
                && constraint.kind == BindingConstraintKind::ObjectLiteralDeclaration
        }));
    }

    #[test]
    fn ast_fact_extractor_projects_commonjs_require_and_exports() {
        let source = r#"
            const answer = require("pkg").answer;
            const later = import("other-pkg");
            exports.answer = answer;
            module.exports.defaultAnswer = answer;
            module.exports = answer;
        "#;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.cjs",
            Some(source.to_string()),
        ));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "m1", "src/module.ts").with_source_file(1));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let graph = RevertsGraph::from_input(&input);
        let exports = graph.import_export().exports_for(ModuleId(1));

        assert!(graph.ast_errors().is_empty());
        assert!(
            graph
                .import_export()
                .package_imports_for(ModuleId(1))
                .contains(&"pkg")
        );
        assert!(
            graph
                .import_export()
                .package_imports_for(ModuleId(1))
                .contains(&"other-pkg")
        );
        assert!(exports.iter().any(|binding| binding.as_str() == "answer"));
        assert!(
            exports
                .iter()
                .any(|binding| binding.as_str() == "defaultAnswer")
        );
    }

    #[test]
    fn ast_fact_extractor_ignores_function_locals_but_keeps_wrapped_module_edges() {
        let source = r#"
            (function () {
                const pkg = require("pkg");
                function inner(param) {
                    let local = param.value;
                    local++;
                    return pkg.make(local);
                }
                exports.answer = inner({ value: 1 });
            })();
        "#;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.cjs",
            Some(source.to_string()),
        ));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "m1", "src/module.ts").with_source_file(1));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let graph = RevertsGraph::from_input(&input);
        let exports = graph.import_export().exports_for(ModuleId(1));

        assert!(graph.ast_errors().is_empty());
        assert!(
            graph
                .import_export()
                .package_imports_for(ModuleId(1))
                .contains(&"pkg")
        );
        assert!(exports.iter().any(|binding| binding.as_str() == "answer"));
        assert!(graph.def_use().unresolved_reads().is_empty());
        assert!(graph.def_use().unresolved_writes().is_empty());
    }

    #[test]
    fn ast_fact_extractor_keeps_top_level_missing_reads_visible() {
        let source = r#"
            function handle(message) {
                const data = message.data;
                return fetch(data.url);
            }
            topLevelMissing();
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
        let unresolved = graph
            .def_use()
            .unresolved_reads()
            .into_iter()
            .map(|(_, binding)| binding.as_str().to_string())
            .collect::<Vec<_>>();

        assert_eq!(unresolved, vec!["topLevelMissing".to_string()]);
    }

    #[test]
    fn ast_fact_extractor_projects_destructured_top_level_bindings() {
        let source = r#"
            const source = { value: 1, other: 2 };
            const list = [1, 2, 3];
            const { value: alias, other, ...rest } = source;
            const [first, , third = 3, ...tail] = list;
            alias; other; rest; first; third; tail;
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
        let definitions = graph
            .definitions_for(ModuleId(1))
            .into_iter()
            .map(|binding| binding.as_str().to_string())
            .collect::<Vec<_>>();

        assert!(graph.ast_errors().is_empty());
        for binding in ["alias", "other", "rest", "first", "third", "tail"] {
            assert!(definitions.contains(&binding.to_string()));
        }
        assert!(graph.def_use().unresolved_reads().is_empty());
    }

    #[test]
    fn ast_fact_extractor_projects_default_exports_and_reexports() {
        let source = r#"
            export { value as renamed } from "pkg/sub";
            export * as everything from "pkg/all";
            export default function run() { return 1; }
        "#;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.mjs",
            Some(source.to_string()),
        ));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "m1", "src/module.ts").with_source_file(1));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let graph = RevertsGraph::from_input(&input);
        let package_imports = graph.import_export().package_imports_for(ModuleId(1));
        let exports = graph.import_export().exports_for(ModuleId(1));

        assert!(graph.ast_errors().is_empty());
        assert!(package_imports.contains(&"pkg/sub"));
        assert!(package_imports.contains(&"pkg/all"));
        for binding in ["default", "everything", "renamed"] {
            assert!(exports.iter().any(|export| export.as_str() == binding));
        }
        assert!(
            graph
                .definitions_for(ModuleId(1))
                .iter()
                .any(|binding| binding.as_str() == "run")
        );
    }

    #[test]
    fn graph_resolves_arbitrary_bundle_prelude_helpers_without_fixed_names() {
        let prelude = concat!(
            "var $wrap7 = (factory, cache) => () => ",
            "(cache || factory((cache = { exports: {} }).exports, cache), cache.exports);\n",
            "var _lazy9 = (init, cache) => () => (init && (cache = init(init = 0)), cache);\n",
        );
        let body = concat!(
            "var entry = $wrap7((exports, module) => { module.exports = 1; });\n",
            "_lazy9();\n",
            "export { entry };\n",
        );
        let source = format!("{prelude}{body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let graph = RevertsGraph::from_input(&input);
        let prelude = graph
            .runtime_prelude(1)
            .expect("bundle prelude should be recovered");
        let runtime_imports = graph.runtime_imports_for(ModuleId(1));
        let runtime_binding_names = runtime_imports
            .iter()
            .map(|import| import.binding.as_str().to_string())
            .collect::<Vec<_>>();

        assert_eq!(
            prelude.binding_kind(&BindingName::new("$wrap7")),
            Some(RuntimePreludeBindingKind::CommonJsWrapper)
        );
        assert_eq!(
            prelude.binding_kind(&BindingName::new("_lazy9")),
            Some(RuntimePreludeBindingKind::LazyInitializer)
        );
        assert_eq!(runtime_binding_names, vec!["$wrap7", "_lazy9"]);
        assert!(graph.def_use().unresolved_reads().is_empty());
    }

    #[test]
    fn graph_recovers_entrypoint_and_minimal_runtime_snippets() {
        let prelude = concat!(
            "var helper = () => dependency();\n",
            "var unused = new Missing(), dependency = () => 1;\n",
            "function main() { return helper(); }\n",
        );
        let body = "export const moduleValue = 1;\n";
        let tail = "main();\n";
        let source = format!("{prelude}{body}{tail}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + body.len()) as u32,
                )),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let graph = RevertsGraph::from_input(&input);
        let prelude = graph
            .runtime_prelude(1)
            .expect("bundle prelude should be recovered");
        let entrypoint = prelude
            .entrypoint
            .as_ref()
            .expect("tail call should be recovered as entrypoint");
        let main = BindingName::new("main");
        let source = prelude.source_for_bindings(std::iter::once(&main));

        assert_eq!(entrypoint.callee.as_str(), "main");
        assert_eq!(entrypoint.statement_source.as_str(), "main();");
        assert!(source.contains("function main()"));
        assert!(source.contains("var helper"));
        assert!(source.contains("var dependency"));
        assert!(!source.contains("unused"));
        assert!(!source.contains("Missing"));
    }

    #[test]
    fn graph_does_not_resolve_bundle_prelude_writes_as_imports() {
        let prelude = "var shared = 0;\n";
        let body = "shared = 1;\n";
        let source = format!("{prelude}{body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let graph = RevertsGraph::from_input(&input);

        assert!(graph.runtime_imports_for(ModuleId(1)).is_empty());
        assert!(
            graph
                .def_use()
                .unresolved_writes()
                .iter()
                .any(|(module_id, binding)| *module_id == ModuleId(1)
                    && binding.as_str() == "shared")
        );
    }

    #[test]
    fn graph_recovers_runtime_declarations_between_module_spans() {
        let first = "var first = 1;\n";
        let runtime = concat!(
            "var $lateRuntime = (factory, cache) => () => ",
            "(cache || factory((cache = { exports: {} }).exports, cache), cache.exports);\n",
        );
        let second = "var second = $lateRuntime((exports, module) => { module.exports = 2; });\n";
        let source = format!("{first}{runtime}{second}");
        let first_start = 0;
        let first_end = first.len() as u32;
        let second_start = (first.len() + runtime.len()) as u32;
        let second_end = source.len() as u32;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "first", "modules/first.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(first_start, first_end)),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "second", "modules/second.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(second_start, second_end)),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let graph = RevertsGraph::from_input(&input);

        assert!(
            graph
                .runtime_prelude(1)
                .expect("runtime prelude should be recovered")
                .defines(&BindingName::new("$lateRuntime"))
        );
        assert!(
            graph
                .runtime_imports_for(ModuleId(2))
                .iter()
                .any(|import| import.binding.as_str() == "$lateRuntime")
        );
        assert!(graph.runtime_imports_for(ModuleId(1)).is_empty());
    }
}
