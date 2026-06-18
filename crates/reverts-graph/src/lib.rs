pub mod fingerprint;
pub use fingerprint::{
    ExtractedFunction, FunctionExtractor, extract_import_specifiers, extract_property_names,
};

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use oxc_allocator::Allocator;
use oxc_ast::{
    Visit,
    ast::{
        Argument, ArrowFunctionExpression, AssignmentExpression, AssignmentTarget, BindingPattern,
        BindingPatternKind, CallExpression, Class, ComputedMemberExpression, Declaration,
        ExportAllDeclaration, ExportDefaultDeclaration, ExportDefaultDeclarationKind,
        ExportNamedDeclaration, Expression, Function, FunctionType, ImportDeclaration,
        ImportDeclarationSpecifier, ImportExpression, ModuleExportName, NewExpression,
        ObjectExpression, ObjectPropertyKind, Program, PropertyKind, SimpleAssignmentTarget,
        Statement, StaticMemberExpression, TSImportType, TSInterfaceHeritage, TSType,
        TSTypeAnnotation, TSTypeParameterInstantiation, UpdateExpression, VariableDeclaration,
        VariableDeclarator,
    },
    visit::walk::{
        walk_arrow_function_expression, walk_call_expression, walk_class,
        walk_computed_member_expression, walk_export_all_declaration,
        walk_export_default_declaration, walk_export_named_declaration, walk_function,
        walk_import_expression, walk_new_expression, walk_static_member_expression,
        walk_ts_import_type, walk_ts_interface_heritage, walk_ts_type, walk_ts_type_annotation,
        walk_ts_type_parameter_instantiation, walk_variable_declarator,
    },
};
use oxc_parser::Parser;
use oxc_span::GetSpan;
use oxc_syntax::{
    operator::{AssignmentOperator, LogicalOperator},
    scope::ScopeFlags,
};
use reverts_input::{InputBundle, ModuleDependencyTarget, ModuleInput, SymbolScope};
use reverts_ir::{
    BindingConstraint, BindingConstraintKind, BindingName, ControlFlowEdgeKind, ControlFlowGraph,
    ControlFlowNodeKind, DefUseGraph, FlowNodeId, ModuleId, ModuleKind, split_bare_specifier,
};
use reverts_js::{
    JsError, ParseError, ParseGoal, collect_identifier_read_facts, lazy_value_sub_snippets,
    parse_error_message, parse_options_for, source_type_candidates, static_property_key_name_ref,
};

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
    pub namespace_exports: Vec<RuntimeNamespaceExport>,
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
        let needed = self.required_bindings_for(bindings);
        let mut snippets = BTreeMap::<u32, String>::new();

        for binding in needed {
            let Some(snippet) = self.snippets.get(&binding) else {
                continue;
            };
            snippets
                .entry(snippet.byte_start)
                .or_insert_with(|| snippet.source.clone());
        }

        snippets.into_values().collect::<Vec<_>>().join("\n")
    }

    #[must_use]
    pub fn required_bindings_for<'a>(
        &self,
        bindings: impl Iterator<Item = &'a BindingName>,
    ) -> BTreeSet<BindingName> {
        let mut needed = bindings.cloned().collect::<BTreeSet<_>>();
        let mut pending = needed.iter().cloned().collect::<VecDeque<_>>();
        let mut visited = BTreeSet::<BindingName>::new();

        while let Some(binding) = pending.pop_front() {
            if !visited.insert(binding.clone()) {
                continue;
            }
            let Some(snippet) = self.snippets.get(&binding) else {
                continue;
            };
            for identifier in runtime_snippet_dependency_identifiers(snippet.source.as_str()) {
                let candidate = BindingName::new(identifier);
                if self.bindings.contains_key(&candidate) && needed.insert(candidate.clone()) {
                    pending.push_back(candidate);
                }
            }
            for namespace_export in &self.namespace_exports {
                if namespace_export.namespace != binding {
                    continue;
                }
                for target in namespace_export.exports.values() {
                    if self.bindings.contains_key(target) && needed.insert(target.clone()) {
                        pending.push_back(target.clone());
                    }
                }
            }
        }

        visited
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePreludeSnippet {
    pub source: String,
    pub byte_start: u32,
    /// Top-level statement-level slices of this snippet, used by the
    /// planner to evaluate migration of individual statements (e.g. each
    /// statement inside a `lazyValue(() => { ... })` body) instead of
    /// the whole snippet. When empty, callers fall back to the whole
    /// `source` as a single unit — preserving the pre-split semantics.
    pub sub_snippets: Vec<RuntimePreludeSubSnippet>,
}

/// A single top-level statement-level slice of a `RuntimePreludeSnippet`.
/// Each sub-snippet covers a contiguous byte range inside the parent
/// snippet and lists the bindings the statement defines / reads / writes
/// so the planner can reason about its individual movability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePreludeSubSnippet {
    pub source: String,
    pub byte_start: u32,
    pub byte_end: u32,
    pub defines: BTreeSet<BindingName>,
    pub reads: BTreeSet<BindingName>,
    pub writes: BTreeSet<BindingName>,
}

impl RuntimePreludeSnippet {
    /// Construct a snippet without any pre-computed sub-snippets. The
    /// planner's lazy-block splitter will populate `sub_snippets` when it
    /// recognises a splittable shape; until then callers treat the
    /// whole `source` as a single unit.
    #[must_use]
    pub fn new(source: impl Into<String>, byte_start: u32) -> Self {
        Self {
            source: source.into(),
            byte_start,
            sub_snippets: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeNamespaceExport {
    pub namespace: BindingName,
    pub helper: BindingName,
    pub exports: BTreeMap<String, BindingName>,
    pub byte_start: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePreludeSideEffect {
    pub source: String,
    pub byte_start: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeEntrypoint {
    pub source_file_id: u32,
    pub callee: BindingName,
    pub statement_source: String,
    pub side_effects: Vec<RuntimePreludeSideEffect>,
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
    /// Optional property name accompanying a member-access constraint. Set
    /// for MemberRead/MemberWrite facts that originate from a static or
    /// computed member expression with a recoverable property name.
    pub property: Option<BindingName>,
}

impl AstFact {
    #[must_use]
    pub fn definition(module_id: ModuleId, binding: impl Into<String>) -> Self {
        Self {
            module_id,
            binding: Some(BindingName::new(binding)),
            kind: AstFactKind::Definition,
            property: None,
        }
    }

    #[must_use]
    pub fn read(module_id: ModuleId, binding: impl Into<String>) -> Self {
        Self {
            module_id,
            binding: Some(BindingName::new(binding)),
            kind: AstFactKind::Read,
            property: None,
        }
    }

    #[must_use]
    pub fn write(module_id: ModuleId, binding: impl Into<String>) -> Self {
        Self {
            module_id,
            binding: Some(BindingName::new(binding)),
            kind: AstFactKind::Write,
            property: None,
        }
    }

    #[must_use]
    pub fn import(module_id: ModuleId, binding: impl Into<String>) -> Self {
        Self {
            module_id,
            binding: Some(BindingName::new(binding)),
            kind: AstFactKind::Import,
            property: None,
        }
    }

    #[must_use]
    pub fn package_import(module_id: ModuleId, specifier: impl Into<String>) -> Self {
        Self {
            module_id,
            binding: Some(BindingName::new(specifier)),
            kind: AstFactKind::PackageImport,
            property: None,
        }
    }

    #[must_use]
    pub fn export(module_id: ModuleId, binding: impl Into<String>) -> Self {
        Self {
            module_id,
            binding: Some(BindingName::new(binding)),
            kind: AstFactKind::Export,
            property: None,
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
            property: None,
        }
    }

    /// Mark `(module_id, binding)` as written from a statically nullable
    /// chain (e.g. `X = (await ...).data.value`). Powers the
    /// `UnprotectedNullableMemberRead` audit; emits no constraint or shape.
    #[must_use]
    pub fn maybe_nullable_write(module_id: ModuleId, binding: impl Into<String>) -> Self {
        Self {
            module_id,
            binding: Some(BindingName::new(binding)),
            kind: AstFactKind::MaybeNullableWrite,
            property: None,
        }
    }

    /// `target = source` identity assignment. Fed into `DefUseGraph`'s
    /// alias closure so downstream propagation can follow renames.
    #[must_use]
    pub fn identity_alias(
        module_id: ModuleId,
        target: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            module_id,
            binding: Some(BindingName::new(target)),
            kind: AstFactKind::IdentityAlias,
            property: Some(BindingName::new(source)),
        }
    }

    /// `target = callee(...)` call-site assignment. Resolved against
    /// `FunctionReturns` facts to compose the alias closure across calls.
    #[must_use]
    pub fn call_alias(
        module_id: ModuleId,
        target: impl Into<String>,
        callee: impl Into<String>,
    ) -> Self {
        Self {
            module_id,
            binding: Some(BindingName::new(target)),
            kind: AstFactKind::CallAlias,
            property: Some(BindingName::new(callee)),
        }
    }

    /// `function F() { return X; }` — F is `function`, X is `returned`.
    #[must_use]
    pub fn function_returns(
        module_id: ModuleId,
        function: impl Into<String>,
        returned: impl Into<String>,
    ) -> Self {
        Self {
            module_id,
            binding: Some(BindingName::new(function)),
            kind: AstFactKind::FunctionReturns,
            property: Some(BindingName::new(returned)),
        }
    }

    /// Construct a member-access constraint fact that records both the
    /// accessed binding and the property name observed on it. Used by the
    /// extractor to populate `BindingConstraint::property` downstream.
    #[must_use]
    pub fn constraint_with_property(
        module_id: ModuleId,
        binding: impl Into<String>,
        kind: BindingConstraintKind,
        property: impl Into<String>,
    ) -> Self {
        Self {
            module_id,
            binding: Some(BindingName::new(binding)),
            kind: AstFactKind::BindingConstraint(kind),
            property: Some(BindingName::new(property)),
        }
    }

    #[must_use]
    pub fn wrapper_region(module_id: ModuleId, kind: AstWrapperKind) -> Self {
        Self {
            module_id,
            binding: None,
            kind: AstFactKind::WrapperRegion(kind),
            property: None,
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
    /// Binding was assigned from a member-access chain on an awaited or
    /// called value (statically nullable RHS). Powers the
    /// `UnprotectedNullableMemberRead` audit.
    MaybeNullableWrite,
    /// `target = source` — `source` is in `AstFact::property`. Feeds the
    /// alias closure on `DefUseGraph` so downstream propagation can follow
    /// identity assignments / renames.
    IdentityAlias,
    /// `target = callee(...)` — `callee` is in `AstFact::property`.
    /// Combined with `FunctionReturns` facts, lets aliased information
    /// flow from a function's return value into the caller binding.
    CallAlias,
    /// `function F() { return X; }` — `function` is in `AstFact::binding`,
    /// `X` (the returned binding) is in `AstFact::property`. Resolves
    /// `CallAlias` edges at closure time.
    FunctionReturns,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
            if symbol.scope != SymbolScope::Module
                || source_backed_modules.contains(&symbol.module_id)
            {
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
                let mut constraint =
                    BindingConstraint::new(fact.module_id, binding.as_str(), *kind);
                if let Some(property) = &fact.property {
                    constraint = constraint.with_property(property.as_str());
                }
                def_use.constrain(constraint);
            }
        }
        AstFactKind::WrapperRegion(_) => {}
        AstFactKind::MaybeNullableWrite => {
            if let Some(binding) = &fact.binding {
                def_use.record_maybe_nullable_write(fact.module_id, binding.as_str());
            }
        }
        AstFactKind::IdentityAlias => {
            if let (Some(target), Some(source)) = (&fact.binding, &fact.property) {
                def_use.record_identity_alias(fact.module_id, target.as_str(), source.as_str());
            }
        }
        AstFactKind::CallAlias => {
            if let (Some(target), Some(callee)) = (&fact.binding, &fact.property) {
                def_use.record_call_alias(fact.module_id, target.as_str(), callee.as_str());
            }
        }
        AstFactKind::FunctionReturns => {
            if let (Some(function), Some(returned)) = (&fact.binding, &fact.property) {
                def_use.record_function_return(
                    fact.module_id,
                    function.as_str(),
                    returned.as_str(),
                );
            }
        }
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
            let collection = collect_runtime_prelude_declarations(
                source_file_id,
                &parsed.program,
                source,
                module_spans,
            );
            if collection.bindings.is_empty() {
                return None;
            }
            return Some(RuntimePrelude {
                source_file_id,
                source_file_path: source_file_path.to_string(),
                source: collection.source,
                bindings: collection.bindings,
                snippets: collection.snippets,
                namespace_exports: collection.namespace_exports,
                entrypoint: collection.entrypoint,
            });
        }
    }

    None
}

struct RuntimePreludeCollection {
    bindings: BTreeMap<BindingName, RuntimePreludeBindingKind>,
    snippets: BTreeMap<BindingName, RuntimePreludeSnippet>,
    namespace_exports: Vec<RuntimeNamespaceExport>,
    source: String,
    entrypoint: Option<RuntimeEntrypoint>,
}

fn collect_runtime_prelude_declarations(
    source_file_id: u32,
    program: &Program<'_>,
    source: &str,
    module_spans: &[(u32, u32)],
) -> RuntimePreludeCollection {
    let mut bindings = BTreeMap::new();
    let mut snippets_by_binding = BTreeMap::new();
    let mut snippets = Vec::new();
    let mut namespace_exports = Vec::new();
    let mut entrypoint_candidate = None;
    let mut entrypoint_side_effects = Vec::new();
    let tail_runtime_start = module_spans.iter().map(|(_, end)| *end).max().unwrap_or(0);
    for statement in &program.body {
        let span = statement.span();
        if !span_outside_module_spans(span.start, span.end, module_spans) {
            continue;
        }
        let namespace_export = runtime_namespace_export_from_statement(statement);
        if namespace_export.is_none()
            && let Some(mut candidate) =
                runtime_entrypoint_from_statement(source_file_id, statement, source)
        {
            candidate.side_effects = entrypoint_side_effects.clone();
            entrypoint_candidate = Some(candidate);
        }
        if let Some(namespace_export) = namespace_export {
            namespace_exports.push(namespace_export);
        }
        let declarations = runtime_prelude_declarations_from_statement(statement, source);
        if declarations.is_empty() {
            if span.start >= tail_runtime_start
                && let Some(side_effect) =
                    runtime_entrypoint_side_effect_from_statement(statement, source)
            {
                entrypoint_side_effects.push(side_effect);
            }
            continue;
        }
        for declaration in declarations {
            let sub_snippets =
                lazy_value_sub_snippets(declaration.source.as_str(), None, ParseGoal::TypeScript)
                    .map(|slices| {
                        slices
                            .into_iter()
                            .map(|slice| RuntimePreludeSubSnippet {
                                source: slice.source,
                                byte_start: slice.byte_start,
                                byte_end: slice.byte_end,
                                defines: slice
                                    .bindings
                                    .iter()
                                    .map(|name| BindingName::new(name.clone()))
                                    .collect(),
                                reads: BTreeSet::new(),
                                writes: BTreeSet::new(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
            bindings.insert(declaration.binding.clone(), declaration.kind);
            snippets_by_binding.insert(
                declaration.binding,
                RuntimePreludeSnippet {
                    source: declaration.source.clone(),
                    byte_start: declaration.byte_start,
                    sub_snippets,
                },
            );
            snippets.push(declaration.source);
        }
    }
    let entrypoint =
        entrypoint_candidate.filter(|candidate| bindings.contains_key(&candidate.callee));
    RuntimePreludeCollection {
        bindings,
        snippets: snippets_by_binding,
        namespace_exports,
        source: snippets.join("\n"),
        entrypoint,
    }
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
        side_effects: Vec::new(),
    })
}

fn runtime_namespace_export_from_statement(
    statement: &Statement<'_>,
) -> Option<RuntimeNamespaceExport> {
    let Statement::ExpressionStatement(statement) = statement else {
        return None;
    };
    let Expression::CallExpression(call) = &statement.expression else {
        return None;
    };
    let kind = namespace_export_call_kind(call)?;
    let namespace = argument_identifier(call.arguments.first()?)?;
    let exports = match kind {
        RuntimeNamespaceExportCallKind::Helper(helper) => {
            let object = argument_object_expression(call.arguments.get(1)?)?;
            namespace_export_object_from_ast(object).map(|exports| (helper, exports))?
        }
        RuntimeNamespaceExportCallKind::ObjectDefineProperties => {
            let object = argument_object_expression(call.arguments.get(1)?)?;
            let exports = namespace_export_object_from_ast(object)?;
            ("Object.defineProperties", exports)
        }
        RuntimeNamespaceExportCallKind::ObjectDefineProperty => {
            let export_name = argument_string_literal(call.arguments.get(1)?)?;
            let descriptor = argument_object_expression(call.arguments.get(2)?)?;
            let target = namespace_property_descriptor_export_target(descriptor)?;
            (
                "Object.defineProperty",
                BTreeMap::from([(export_name.to_string(), BindingName::new(target))]),
            )
        }
    };
    let (helper, exports) = exports;
    if exports.is_empty() {
        return None;
    }
    Some(RuntimeNamespaceExport {
        namespace: BindingName::new(namespace),
        helper: BindingName::new(helper),
        exports,
        byte_start: statement.span().start,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeNamespaceExportCallKind<'a> {
    Helper(&'a str),
    ObjectDefineProperties,
    ObjectDefineProperty,
}

fn namespace_export_call_kind<'a>(
    call: &'a CallExpression<'a>,
) -> Option<RuntimeNamespaceExportCallKind<'a>> {
    namespace_export_callee_kind(&call.callee)
}

fn namespace_export_callee_kind<'a>(
    callee: &'a Expression<'a>,
) -> Option<RuntimeNamespaceExportCallKind<'a>> {
    match callee {
        Expression::Identifier(callee) => {
            Some(RuntimeNamespaceExportCallKind::Helper(callee.name.as_str()))
        }
        Expression::StaticMemberExpression(member) => {
            if expression_identifier(&member.object) != Some("Object") {
                return None;
            }
            match member.property.name.as_str() {
                "defineProperties" => Some(RuntimeNamespaceExportCallKind::ObjectDefineProperties),
                "defineProperty" => Some(RuntimeNamespaceExportCallKind::ObjectDefineProperty),
                _ => None,
            }
        }
        Expression::ParenthesizedExpression(parenthesized) => {
            namespace_export_callee_kind(&parenthesized.expression)
        }
        _ => None,
    }
}

fn argument_identifier<'a>(argument: &'a Argument<'a>) -> Option<&'a str> {
    match argument {
        Argument::Identifier(identifier) => Some(identifier.name.as_str()),
        Argument::ParenthesizedExpression(parenthesized) => {
            expression_identifier(&parenthesized.expression)
        }
        _ => None,
    }
}

fn argument_object_expression<'a>(argument: &'a Argument<'a>) -> Option<&'a ObjectExpression<'a>> {
    match argument {
        Argument::ObjectExpression(object) => Some(object),
        Argument::ParenthesizedExpression(parenthesized) => {
            expression_object_expression(&parenthesized.expression)
        }
        _ => None,
    }
}

fn expression_object_expression<'a>(
    expression: &'a Expression<'a>,
) -> Option<&'a ObjectExpression<'a>> {
    match expression {
        Expression::ObjectExpression(object) => Some(object),
        Expression::ParenthesizedExpression(parenthesized) => {
            expression_object_expression(&parenthesized.expression)
        }
        _ => None,
    }
}

fn namespace_export_object_from_ast(
    object: &ObjectExpression<'_>,
) -> Option<BTreeMap<String, BindingName>> {
    let mut exports = BTreeMap::new();
    for property in &object.properties {
        let ObjectPropertyKind::ObjectProperty(property) = property else {
            continue;
        };
        if property.kind != PropertyKind::Init || property.computed {
            continue;
        }
        let Some(export_name) = property.key.static_name() else {
            continue;
        };
        let Some(target) = namespace_export_target(&property.value) else {
            continue;
        };
        exports.insert(export_name.into_owned(), BindingName::new(target));
    }
    Some(exports)
}

fn namespace_export_target<'a>(expression: &'a Expression<'a>) -> Option<&'a str> {
    match expression {
        Expression::ArrowFunctionExpression(arrow) => function_body_return_identifier(&arrow.body),
        Expression::FunctionExpression(function) => function
            .body
            .as_ref()
            .and_then(|body| function_body_return_identifier(body)),
        Expression::ObjectExpression(object) => namespace_property_descriptor_export_target(object),
        Expression::ParenthesizedExpression(parenthesized) => {
            namespace_export_target(&parenthesized.expression)
        }
        _ => expression_identifier(expression),
    }
}

fn namespace_property_descriptor_export_target<'a>(
    object: &'a ObjectExpression<'a>,
) -> Option<&'a str> {
    for property in &object.properties {
        let ObjectPropertyKind::ObjectProperty(property) = property else {
            continue;
        };
        if property.kind != PropertyKind::Init || property.computed {
            continue;
        }
        let Some("get") = property.key.static_name().as_deref() else {
            continue;
        };
        return namespace_export_target(&property.value);
    }
    None
}

fn function_body_return_identifier<'a>(
    body: &'a oxc_ast::ast::FunctionBody<'a>,
) -> Option<&'a str> {
    let [statement] = body.statements.as_slice() else {
        return None;
    };
    match statement {
        Statement::ExpressionStatement(statement) => expression_identifier(&statement.expression),
        Statement::ReturnStatement(statement) => {
            statement.argument.as_ref().and_then(expression_identifier)
        }
        _ => None,
    }
}

fn runtime_entrypoint_side_effect_from_statement(
    statement: &Statement<'_>,
    source: &str,
) -> Option<RuntimePreludeSideEffect> {
    if runtime_namespace_export_from_statement(statement).is_some() {
        return None;
    }
    match statement {
        Statement::VariableDeclaration(_)
        | Statement::FunctionDeclaration(_)
        | Statement::ClassDeclaration(_)
        | Statement::ImportDeclaration(_)
        | Statement::ExportNamedDeclaration(_)
        | Statement::ExportDefaultDeclaration(_)
        | Statement::ExportAllDeclaration(_) => return None,
        _ => {}
    }
    let span = statement.span();
    let source = source
        .get(span.start as usize..span.end as usize)?
        .to_string();
    Some(RuntimePreludeSideEffect {
        source,
        byte_start: span.start,
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

fn runtime_snippet_dependency_identifiers(source: &str) -> BTreeSet<String> {
    collect_identifier_read_facts(source, None, ParseGoal::TypeScript)
        .expect("runtime prelude snippets require parseable TypeScript source")
        .into_iter()
        .map(|fact| fact.name)
        .collect()
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
                    function_stack: Vec::new(),
                    type_position_depth: 0,
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

struct AstFactVisitor {
    module_id: ModuleId,
    facts: Vec<AstFact>,
    function_depth: usize,
    module_scope_bindings: BTreeSet<String>,
    /// Names of enclosing function declarations. `return X` looks at the
    /// top of the stack to determine which function aliases `X`.
    function_stack: Vec<String>,
    /// Depth counter for TypeScript type positions. Identifier references
    /// inside type annotations (`Record<K, V>`, `interface X extends Y`,
    /// `foo<T>()`, etc.) are not value reads and must not be recorded as
    /// such — otherwise they leak into `unresolved_reads` and the audit
    /// flags real type-only utility names like `Record` or `Partial`.
    type_position_depth: usize,
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
            // Paper #7 — `const { foo, bar } = ns;` is a property access on
            // `ns`. Record each statically recoverable key so the namespace
            // surface sees what the destructuring pattern consumed.
            if let Some(init) = &it.init
                && let Some(rhs) = direct_member_object(init)
                && let BindingPatternKind::ObjectPattern(pattern) = &it.id.kind
            {
                for property in &pattern.properties {
                    if property.computed {
                        continue;
                    }
                    if let Some(name) = static_property_key_name_ref(&property.key) {
                        self.member_constraint(rhs, BindingConstraintKind::MemberRead, name);
                    }
                }
            }
            // Alias propagation — `let A = X` (identity) or `let A = F()`
            // (call). Single-binding declarators only; destructuring would
            // need element-wise tracking and is out of scope here.
            if bindings.len() == 1
                && let Some(init) = &it.init
            {
                self.maybe_record_alias_for_assignment(bindings[0], init);
            }
        }
        walk_variable_declarator(self, it);
    }

    fn visit_return_statement(&mut self, it: &oxc_ast::ast::ReturnStatement<'a>) {
        if self.function_depth >= 1
            && let Some(argument) = &it.argument
            && let Some(returned) = direct_identifier(argument)
            && let Some(function_name) = self.function_stack.last()
        {
            // Only track when returned binding is module-scope (it has to
            // be visible to callers' alias lookup) and we're inside a
            // top-level function declaration.
            if self.function_depth == 1 && self.module_scope_bindings.contains(returned) {
                self.facts.push(AstFact::function_returns(
                    self.module_id,
                    function_name.as_str(),
                    returned,
                ));
            }
        }
        oxc_ast::visit::walk::walk_return_statement(self, it);
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

        let pushed_function = if it.r#type == FunctionType::FunctionDeclaration
            && let Some(id) = &it.id
        {
            self.function_stack.push(id.name.to_string());
            true
        } else {
            false
        };
        self.function_depth += 1;
        walk_function(self, it, flags);
        self.function_depth -= 1;
        if pushed_function {
            self.function_stack.pop();
        }
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        self.function_depth += 1;
        walk_arrow_function_expression(self, it);
        self.function_depth -= 1;
    }

    fn visit_catch_clause(&mut self, it: &oxc_ast::ast::CatchClause<'a>) {
        // A `catch (E) { … }` clause introduces `E` for the body. Without
        // recording it as a binding the body's reads of `E` look like
        // unresolved module-scope reads (the catch body itself is at
        // function_depth == 0 when the try/catch is top-level).
        if let Some(param) = &it.param {
            for name in binding_pattern_names(&param.pattern) {
                self.definition(name);
            }
        }
        oxc_ast::visit::walk::walk_catch_clause(self, it);
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
                if let Some((direct_binding, property)) =
                    direct_assignment_member_property(&it.left)
                {
                    self.member_constraint(
                        direct_binding,
                        BindingConstraintKind::MemberWrite,
                        property,
                    );
                } else {
                    self.constraint(binding, BindingConstraintKind::MemberWrite);
                }
            } else if it.operator.is_assign() && expression_is_maybe_nullable(&it.right) {
                // Plain `X = expr` where the RHS is a member chain on an
                // awaited/called value: X is statically nullable from now on.
                // Powers the `UnprotectedNullableMemberRead` audit.
                self.maybe_nullable_write(binding);
            }
            // Alias propagation also picks up plain `A = X` and `A = F()`.
            // Member-target writes (`A.foo = ...`) have no propagatable
            // identity from the perspective of `A` itself.
            if it.operator.is_assign() && !assignment_target_is_member(&it.left) {
                self.maybe_record_alias_for_assignment(binding, &it.right);
            }
        }

        self.visit_expression(&it.right);
    }

    fn visit_update_expression(&mut self, it: &UpdateExpression<'a>) {
        if let Some(binding) = simple_assignment_target_identifier(&it.argument) {
            self.read(binding);
            self.write(binding);
            if simple_assignment_target_is_member(&it.argument) {
                if let Some((direct_binding, property)) =
                    direct_simple_assignment_member_property(&it.argument)
                {
                    self.member_constraint(
                        direct_binding,
                        BindingConstraintKind::MemberWrite,
                        property,
                    );
                } else {
                    self.constraint(binding, BindingConstraintKind::MemberWrite);
                }
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
            let augmented_binding = it.arguments.first().and_then(enum_initializer_binding);
            match augmented_binding {
                Some(binding) if iife_body_has_enum_init_pattern(&it.callee, binding) => {
                    self.constraint(binding, BindingConstraintKind::EnumInitializer);
                    self.facts.push(AstFact {
                        module_id: self.module_id,
                        binding: Some(BindingName::new(binding)),
                        kind: AstFactKind::WrapperRegion(AstWrapperKind::EnumIife),
                        property: None,
                    });
                }
                Some(_) => {
                    // TypeScript namespace augmentation: `(function(X){...})(X || (X = {}))`
                    // with a non-enum body. Not a bundler IIFE — emit no wrapper region.
                }
                None => {
                    self.facts
                        .push(AstFact::wrapper_region(self.module_id, kind));
                }
            }
        }
        // Member-access tracking on the callee (e.g. `ns.foo()`) is
        // already handled by `visit_static_member_expression` /
        // `visit_computed_member_expression` via the walk below, which now
        // also records the property name (paper #7 downstream). No need to
        // double-record here.
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
        if let Some(binding) = direct_member_object(&it.object) {
            self.member_constraint(
                binding,
                BindingConstraintKind::MemberRead,
                it.property.name.as_str(),
            );
        }
        walk_static_member_expression(self, it);
    }

    fn visit_ts_type(&mut self, it: &TSType<'a>) {
        self.type_position_depth += 1;
        walk_ts_type(self, it);
        self.type_position_depth -= 1;
    }

    fn visit_ts_type_annotation(&mut self, it: &TSTypeAnnotation<'a>) {
        self.type_position_depth += 1;
        walk_ts_type_annotation(self, it);
        self.type_position_depth -= 1;
    }

    fn visit_ts_type_parameter_instantiation(&mut self, it: &TSTypeParameterInstantiation<'a>) {
        self.type_position_depth += 1;
        walk_ts_type_parameter_instantiation(self, it);
        self.type_position_depth -= 1;
    }

    fn visit_ts_interface_heritage(&mut self, it: &TSInterfaceHeritage<'a>) {
        self.type_position_depth += 1;
        walk_ts_interface_heritage(self, it);
        self.type_position_depth -= 1;
    }

    fn visit_ts_import_type(&mut self, it: &TSImportType<'a>) {
        self.type_position_depth += 1;
        walk_ts_import_type(self, it);
        self.type_position_depth -= 1;
    }

    fn visit_computed_member_expression(&mut self, it: &ComputedMemberExpression<'a>) {
        if let Some(binding) = direct_member_object(&it.object) {
            // Computed key may be a string literal (e.g. `ns["foo"]`) — capture
            // it when we can recover a property name. Other expressions
            // (numeric index, dynamic) record the constraint without a
            // property name so the shape constraint still applies.
            if let Expression::StringLiteral(literal) = &it.expression {
                self.member_constraint(
                    binding,
                    BindingConstraintKind::MemberRead,
                    literal.value.as_str(),
                );
            } else {
                self.constraint(binding, BindingConstraintKind::MemberRead);
            }
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

    fn member_constraint(&mut self, binding: &str, kind: BindingConstraintKind, property: &str) {
        if self.should_emit_binding_fact(binding) {
            self.facts.push(AstFact::constraint_with_property(
                self.module_id,
                binding,
                kind,
                property,
            ));
        }
    }

    fn maybe_nullable_write(&mut self, binding: &str) {
        if self.should_emit_binding_fact(binding) {
            self.facts
                .push(AstFact::maybe_nullable_write(self.module_id, binding));
        }
    }

    /// Emit `IdentityAlias` for `target = source_identifier` or
    /// `CallAlias` for `target = callee_identifier(args)`. Other RHS
    /// shapes are left alone — they have no usable alias edge.
    fn maybe_record_alias_for_assignment(&mut self, target: &str, init: &Expression<'_>) {
        if let Some(source) = direct_identifier(init) {
            if source != target {
                self.facts
                    .push(AstFact::identity_alias(self.module_id, target, source));
            }
            return;
        }
        if let Expression::CallExpression(call) = init
            && let Some(callee) = direct_identifier(&call.callee)
        {
            self.facts
                .push(AstFact::call_alias(self.module_id, target, callee));
        }
    }

    fn should_emit_binding_fact(&self, binding: &str) -> bool {
        if self.type_position_depth > 0 {
            return false;
        }
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

/// True when `expr` is a member-access chain rooted in a call /
/// await / parenthesised call — the shape used by patterns like
/// `(await fetch(...)).data.value` or `obj.fn().result.field`. The chain's
/// terminal property can resolve to `null`/`undefined`, so any binding
/// receiving such an RHS becomes statically nullable from that point on.
fn expression_is_maybe_nullable(expr: &Expression<'_>) -> bool {
    match expr {
        Expression::StaticMemberExpression(member) => {
            expression_chain_root_is_call_like(&member.object)
        }
        Expression::ComputedMemberExpression(member) => {
            expression_chain_root_is_call_like(&member.object)
        }
        Expression::ChainExpression(chain) => {
            // `obj?.data?.value` — same nullable-chain risk on the leaf
            // binding even though the chain itself is null-safe at each
            // step.
            chain_expression_root_is_call_like(chain)
        }
        Expression::ParenthesizedExpression(inner) => {
            expression_is_maybe_nullable(&inner.expression)
        }
        Expression::TSAsExpression(inner) => expression_is_maybe_nullable(&inner.expression),
        Expression::TSSatisfiesExpression(inner) => expression_is_maybe_nullable(&inner.expression),
        Expression::TSNonNullExpression(inner) => expression_is_maybe_nullable(&inner.expression),
        _ => false,
    }
}

fn expression_chain_root_is_call_like(expr: &Expression<'_>) -> bool {
    match expr {
        Expression::CallExpression(_) | Expression::AwaitExpression(_) => true,
        Expression::ParenthesizedExpression(inner) => {
            expression_chain_root_is_call_like(&inner.expression)
        }
        Expression::TSAsExpression(inner) => expression_chain_root_is_call_like(&inner.expression),
        Expression::TSSatisfiesExpression(inner) => {
            expression_chain_root_is_call_like(&inner.expression)
        }
        Expression::TSNonNullExpression(inner) => {
            expression_chain_root_is_call_like(&inner.expression)
        }
        Expression::StaticMemberExpression(member) => {
            expression_chain_root_is_call_like(&member.object)
        }
        Expression::ComputedMemberExpression(member) => {
            expression_chain_root_is_call_like(&member.object)
        }
        _ => false,
    }
}

fn chain_expression_root_is_call_like(chain: &oxc_ast::ast::ChainExpression<'_>) -> bool {
    use oxc_ast::ast::ChainElement;
    match &chain.expression {
        ChainElement::CallExpression(_) => true,
        ChainElement::StaticMemberExpression(member) => {
            expression_chain_root_is_call_like(&member.object)
        }
        ChainElement::ComputedMemberExpression(member) => {
            expression_chain_root_is_call_like(&member.object)
        }
        ChainElement::TSNonNullExpression(inner) => {
            expression_chain_root_is_call_like(&inner.expression)
        }
        _ => false,
    }
}

/// Match an expression that is *exactly* a plain identifier (after
/// stripping parens and TS-only wrappers). Unlike `expression_identifier`,
/// this does NOT chase through member accesses — the caller needs to know
/// the identity is direct, not "the leftmost name of a member chain".
fn direct_identifier<'a>(expression: &'a Expression<'a>) -> Option<&'a str> {
    match expression {
        Expression::Identifier(identifier) => Some(identifier.name.as_str()),
        Expression::ParenthesizedExpression(inner) => direct_identifier(&inner.expression),
        Expression::TSAsExpression(inner) => direct_identifier(&inner.expression),
        Expression::TSSatisfiesExpression(inner) => direct_identifier(&inner.expression),
        Expression::TSNonNullExpression(inner) => direct_identifier(&inner.expression),
        Expression::TSTypeAssertion(inner) => direct_identifier(&inner.expression),
        Expression::TSInstantiationExpression(inner) => direct_identifier(&inner.expression),
        _ => None,
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

/// Like [`expression_identifier`] but stops at member expressions — `ns.foo`
/// resolves to `Some("ns")`, while `ns.foo.bar` resolves to `None` so the
/// outer access never attributes `bar` to `ns`. Used by the member-access
/// visitors so property names only attach to the binding that immediately
/// owns them.
fn direct_member_object<'a>(expression: &'a Expression<'a>) -> Option<&'a str> {
    match expression {
        Expression::Identifier(identifier) => Some(identifier.name.as_str()),
        Expression::ParenthesizedExpression(parenthesized) => {
            direct_member_object(&parenthesized.expression)
        }
        Expression::TSAsExpression(expression) => direct_member_object(&expression.expression),
        Expression::TSSatisfiesExpression(expression) => {
            direct_member_object(&expression.expression)
        }
        Expression::TSNonNullExpression(expression) => direct_member_object(&expression.expression),
        Expression::TSTypeAssertion(expression) => direct_member_object(&expression.expression),
        Expression::TSInstantiationExpression(expression) => {
            direct_member_object(&expression.expression)
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

fn iife_body_has_enum_init_pattern(callee: &Expression<'_>, binding: &str) -> bool {
    let statements = match callee {
        Expression::FunctionExpression(function) => function
            .body
            .as_ref()
            .map(|body| body.statements.as_slice()),
        Expression::ArrowFunctionExpression(arrow) => Some(arrow.body.statements.as_slice()),
        Expression::ParenthesizedExpression(parenthesized) => {
            return iife_body_has_enum_init_pattern(&parenthesized.expression, binding);
        }
        _ => None,
    };
    let Some(statements) = statements else {
        return false;
    };
    statements
        .iter()
        .any(|statement| statement_is_enum_reverse_mapping(statement, binding))
}

fn statement_is_enum_reverse_mapping(statement: &Statement<'_>, binding: &str) -> bool {
    let Statement::ExpressionStatement(statement) = statement else {
        return false;
    };
    let Expression::AssignmentExpression(outer) = &statement.expression else {
        return false;
    };
    if outer.operator != AssignmentOperator::Assign {
        return false;
    }
    let AssignmentTarget::ComputedMemberExpression(computed) = &outer.left else {
        return false;
    };
    if expression_identifier(&computed.object) != Some(binding) {
        return false;
    }
    let Expression::AssignmentExpression(inner) = &computed.expression else {
        return false;
    };
    if inner.operator != AssignmentOperator::Assign {
        return false;
    }
    let inner_object = match &inner.left {
        AssignmentTarget::StaticMemberExpression(member) => &member.object,
        AssignmentTarget::ComputedMemberExpression(member) => &member.object,
        _ => return false,
    };
    expression_identifier(inner_object) == Some(binding)
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

/// Extract `(binding, property)` from an assignment target when the
/// target is `<identifier>.<prop>` or `<identifier>["<prop>"]` — i.e. the
/// property name is recoverable AND the object is a direct identifier
/// (paralleling `direct_member_object`). Chained writes like
/// `ns.foo.deep = 1` resolve to `None` so `deep` never bleeds onto `ns`.
fn direct_assignment_member_property<'a>(
    target: &'a AssignmentTarget<'a>,
) -> Option<(&'a str, &'a str)> {
    match target {
        AssignmentTarget::StaticMemberExpression(member) => {
            let binding = direct_member_object(&member.object)?;
            Some((binding, member.property.name.as_str()))
        }
        AssignmentTarget::ComputedMemberExpression(member) => {
            let binding = direct_member_object(&member.object)?;
            if let Expression::StringLiteral(literal) = &member.expression {
                Some((binding, literal.value.as_str()))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// `SimpleAssignmentTarget` variant of [`direct_assignment_member_property`]
/// — used by `visit_update_expression` (`++` / `--`).
fn direct_simple_assignment_member_property<'a>(
    target: &'a SimpleAssignmentTarget<'a>,
) -> Option<(&'a str, &'a str)> {
    match target {
        SimpleAssignmentTarget::StaticMemberExpression(member) => {
            let binding = direct_member_object(&member.object)?;
            Some((binding, member.property.name.as_str()))
        }
        SimpleAssignmentTarget::ComputedMemberExpression(member) => {
            let binding = direct_member_object(&member.object)?;
            if let Expression::StringLiteral(literal) = &member.expression {
                Some((binding, literal.value.as_str()))
            } else {
                None
            }
        }
        _ => None,
    }
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

/// Recognises the three IIFE wrapper shapes the graph uses for top-level
/// program scans: `(function () { … })()`, `(() => { … })()`, and the
/// TypeScript-style `var X; (function (X) { … })(X || (X = {}));`
/// initialiser. Exposed `pub` so `reverts-bundle::classifier` can reuse
/// the same predicate and avoid a two-track implementation.
pub fn iife_kind(expression: &Expression<'_>) -> Option<AstWrapperKind> {
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
    fn ast_fact_extractor_does_not_misclassify_namespace_iife_as_enum() {
        // Classic TypeScript "namespace augmentation": the IIFE has the
        // (X || (X = {})) argument shape, but the body attaches properties
        // and functions instead of the X[X.K=N]="K" enum reverse-mapping.
        let source = r#"
            var Logger;
            (function (Logger) {
                Logger.level = "info";
                function log(message) { console.log(message); }
                Logger.log = log;
            })(Logger || (Logger = {}));
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
        assert!(
            !graph.def_use().constraints().iter().any(|constraint| {
                constraint.binding.as_str() == "Logger"
                    && constraint.kind == BindingConstraintKind::EnumInitializer
            }),
            "namespace augmentation IIFE must not be classified as an enum-initializer",
        );
        assert!(
            !graph.ast_facts().iter().any(|fact| {
                fact.binding
                    .as_ref()
                    .is_some_and(|binding| binding.as_str() == "Logger")
                    && fact.kind == AstFactKind::WrapperRegion(AstWrapperKind::EnumIife)
            }),
            "namespace augmentation IIFE must not produce an EnumIife wrapper region",
        );
        assert!(
            !graph.ast_facts().iter().any(|fact| matches!(
                fact.kind,
                AstFactKind::WrapperRegion(AstWrapperKind::FunctionIife)
            )),
            "namespace augmentation IIFE must not be tagged as a generic bundler IIFE wrapper",
        );
    }

    #[test]
    fn ast_fact_extractor_records_property_names_on_member_access_constraints() {
        // Paper #7 plumbing: `ns.foo` / `ns["bar"]` member accesses must
        // surface as constraints carrying the property name. The DefUseGraph
        // query `members_accessed_on` then exposes the deduplicated set.
        let source = r#"
            const ns = {};
            ns.foo;
            ns["bar"];
            ns.foo;
            ns[dynamicKey];
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

        let members = graph.def_use().members_accessed_on(ModuleId(1), "ns");
        let names: Vec<&str> = members.iter().map(BindingName::as_str).collect();
        assert_eq!(names, vec!["bar", "foo"]);
    }

    #[test]
    fn ast_fact_extractor_records_member_call_property_through_static_member_visitor() {
        // `ns.foo()` should still record `foo` as a member of `ns` — the
        // static-member walker handles it, so `visit_call_expression` does
        // not need its own member-access branch. Pinning this so we can
        // delete the redundant branch with confidence.
        let source = r#"
            const ns = {};
            ns.foo();
            ns["bar"]();
            ns.foo.deep();
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

        let members = graph.def_use().members_accessed_on(ModuleId(1), "ns");
        let names: Vec<&str> = members.iter().map(BindingName::as_str).collect();
        // `bar` and `foo` are direct properties; `deep` belongs to `ns.foo`
        // (chained) and must not bleed back onto `ns`.
        assert_eq!(names, vec!["bar", "foo"]);
    }

    #[test]
    fn ast_fact_extractor_records_identity_alias_and_call_alias_edges() {
        // `let A = X;` and `let B = F();` should populate the IR's alias
        // graph so the closure can propagate shape/members/nullable info
        // through renames and call composition.
        let source = r#"
            var X;
            function F() { return X; }
            let A = X;
            let B = F();
            A;
            B;
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

        let aliases_of_a = graph.def_use().alias_sources_of(ModuleId(1), "A");
        let names_a: Vec<_> = aliases_of_a.iter().map(BindingName::as_str).collect();
        assert!(names_a.contains(&"A"));
        assert!(names_a.contains(&"X"));

        let aliases_of_b = graph.def_use().alias_sources_of(ModuleId(1), "B");
        let names_b: Vec<_> = aliases_of_b.iter().map(BindingName::as_str).collect();
        assert!(names_b.contains(&"B"));
        assert!(
            names_b.contains(&"X"),
            "call alias should resolve via F's return to X; got {:?}",
            names_b
        );
    }

    #[test]
    fn ast_fact_extractor_records_known_members_from_object_destructuring() {
        // `const { foo, bar } = ns;` is semantically equivalent to
        // `const foo = ns.foo; const bar = ns.bar;` — the user is accessing
        // those specific members. The solver should see them so paper #7
        // downstream can keep the namespace surface accurate when the
        // emitted bundle uses destructuring instead of dot access.
        let source = r#"
            const ns = { foo: 1, bar: 2, qux: 3 };
            const { foo, bar } = ns;
            const { qux: localQux = 9 } = ns;
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

        let members = graph.def_use().members_accessed_on(ModuleId(1), "ns");
        let names: Vec<&str> = members.iter().map(BindingName::as_str).collect();
        assert_eq!(names, vec!["bar", "foo", "qux"]);
    }

    #[test]
    fn ast_fact_extractor_records_known_members_through_optional_chaining() {
        // `ns?.foo` and `ns?.['bar']` should record `foo` and `bar` as
        // properties of `ns`, just like the non-optional forms.
        let source = r#"
            const ns = {};
            ns?.foo;
            ns?.['bar'];
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

        let members = graph.def_use().members_accessed_on(ModuleId(1), "ns");
        let names: Vec<&str> = members.iter().map(BindingName::as_str).collect();
        assert_eq!(names, vec!["bar", "foo"]);
    }

    #[test]
    fn ast_fact_extractor_records_property_names_on_member_write_constraints() {
        // Symmetric to MemberRead: writing through a member access (`ns.foo
        // = 1`, `ns["bar"] = 2`, `ns.foo++`) must record the property name
        // so `members_accessed_on` exposes it for paper #7 downstream.
        // Chained writes (`ns.foo.deep = 3`) must NOT attribute `deep` back
        // to `ns` — it belongs to `ns.foo`.
        let source = r#"
            const ns = {};
            ns.foo = 1;
            ns["bar"] = 2;
            ns.foo.deep = 3;
            ns.qux++;
            ns[dynamicKey] = 4;
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

        let members = graph.def_use().members_accessed_on(ModuleId(1), "ns");
        let names: Vec<&str> = members.iter().map(BindingName::as_str).collect();
        // `deep` belongs to ns.foo, not ns; `dynamicKey` is computed without
        // a string-literal key, so no property name is recoverable for it.
        assert_eq!(names, vec!["bar", "foo", "qux"]);
    }

    #[test]
    fn ast_fact_extractor_stops_at_chained_access_through_parens_and_ts_wrappers() {
        // Parenthesised and TS-cast expressions wrapping a member access
        // should not "see through" the member to attach a deeper property
        // back onto the original binding. The `direct_member_object` helper
        // must accept identifier-through-wrappers but stop at member exprs.
        let source = r#"
            const ns = {};
            (ns).foo.bar;
            (ns as any).foo.baz;
            ((ns)!).foo.qux;
        "#;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.ts",
            Some(source.to_string()),
        ));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "m1", "src/module.ts").with_source_file(1));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let graph = RevertsGraph::from_input(&input);

        let members = graph.def_use().members_accessed_on(ModuleId(1), "ns");
        let names: Vec<&str> = members.iter().map(BindingName::as_str).collect();
        // Only `foo` is a direct property of `ns`; `bar`, `baz`, `qux` are
        // properties of `(ns…).foo` regardless of the wrapping form.
        assert_eq!(names, vec!["foo"]);
    }

    #[test]
    fn ast_fact_extractor_only_records_direct_property_on_chained_member_access() {
        // Regression: `expression_identifier` recurses through nested member
        // expressions, so before this fix `ns.foo.bar` recorded `bar` as a
        // direct property of `ns` (it was actually a property of `ns.foo`).
        // The corpus surfaced this via the esbuild `import_fs.default.X`
        // pattern — `X` was being attributed to `import_fs` instead of
        // `import_fs.default`. Only the immediate object's property name
        // belongs to that binding.
        let source = r#"
            const ns = {};
            ns.foo;
            ns.foo.bar;
            ns["foo"].baz;
            ns.foo["qux"];
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

        let members = graph.def_use().members_accessed_on(ModuleId(1), "ns");
        let names: Vec<&str> = members.iter().map(BindingName::as_str).collect();
        // `foo` is the only direct property of `ns`. `bar`, `baz`, `qux` are
        // properties of `ns.foo` and must not bleed onto `ns`.
        assert_eq!(names, vec!["foo"]);
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
                recover();
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
    fn ast_fact_extractor_treats_catch_parameter_as_a_binding() {
        let source = r#"
            let flag = false;
            try {
                doThing();
            } catch (err) {
                flag = err.code === 'ERR';
            }
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

        assert!(graph.ast_errors().is_empty());
        assert!(
            !unresolved.contains(&"err".to_string()),
            "catch parameter `err` must not leak as an unresolved read: {unresolved:?}",
        );
        // `doThing` is genuinely unresolved (no import, no definition) — keep this assertion
        // so the test fails if the scoping fix accidentally suppresses real missing bindings.
        assert!(
            unresolved.contains(&"doThing".to_string()),
            "real unresolved binding `doThing` should still surface: {unresolved:?}",
        );
    }

    #[test]
    fn ast_fact_extractor_skips_identifier_reads_in_typescript_type_positions() {
        let source = r#"
            type Alias<T> = Record<string, T>;
            interface Shape extends Pick<SomeOther, "a" | "b"> {
                readonly value: Partial<Map<string, number>>;
            }
            export const make = (input: Omit<Shape, "value">): ReturnType<typeof builder> => {
                return builder(input);
            };
            function builder(_: unknown) { return { value: undefined }; }
        "#;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.ts",
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

        assert!(graph.ast_errors().is_empty());
        // `Record`, `Pick`, `Partial`, `Map`, `Omit`, `ReturnType`, `Shape`, `SomeOther`
        // appear only in type positions and must not register as value reads.
        for type_only in [
            "Record",
            "Pick",
            "Partial",
            "Map",
            "Omit",
            "ReturnType",
            "Shape",
            "SomeOther",
            "Alias",
        ] {
            assert!(
                !unresolved.contains(&type_only.to_string()),
                "type-only binding {type_only} leaked into unresolved value reads: {unresolved:?}",
            );
        }
    }

    #[test]
    fn ast_fact_extractor_skips_advanced_typescript_type_positions() {
        // Exercise every advanced TS construct that wraps an identifier in
        // type position: conditional types, mapped types, `infer` in
        // conditionals, type predicates (`x is T`), `typeof` queries inside
        // a type, class type-parameter constraints, function-type return
        // annotations, and method signatures. A regression in any one of
        // these would leak the corresponding type-only identifier into the
        // unresolved-value-reads stream and re-trigger the false-positive
        // audit findings we removed.
        let source = r#"
            type Conditional<T> = T extends MarkerString ? T : MarkerNever;
            type Mapped<T> = { [K in keyof T]: ValueOf<T[K]> };
            type InferReturn<T> = T extends (...args: any) => infer InferredR ? InferredR : never;
            type TupleHead<T> = T extends [infer InferredH, ...any[]] ? InferredH : never;
            type ValueOf<T> = T[keyof T];

            function isMarkerThing(x: unknown): x is MarkerThing {
                return typeof x === "object";
            }

            const t: typeof externalValue = externalValue;

            class Generic<T extends BaseConstraint> {
                describe(value: T): MarkerString { return ""; }
            }

            type Callback = (input: CallbackInput) => CallbackOutput;
            interface Bag { method(arg: BagArg): BagResult; }

            export const externalValue = 1;
            export { isMarkerThing, Generic };
        "#;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.ts",
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

        assert!(graph.ast_errors().is_empty());
        for type_only in [
            // local type aliases — declarations, not reads
            "Conditional",
            "Mapped",
            "InferReturn",
            "TupleHead",
            "ValueOf",
            "Callback",
            "Bag",
            // type-only ambients the bundle references
            "MarkerString",
            "MarkerNever",
            "MarkerThing",
            "BaseConstraint",
            "CallbackInput",
            "CallbackOutput",
            "BagArg",
            "BagResult",
            // `infer X` introduces a type-position binding that should never
            // leak as a value read either
            "InferredR",
            "InferredH",
        ] {
            assert!(
                !unresolved.contains(&type_only.to_string()),
                "type-only binding {type_only} leaked into unresolved value reads: {unresolved:?}",
            );
        }
        // `typeof externalValue` references a value binding — `externalValue`
        // appears in real value position on the next line too, so it must
        // resolve normally and not leak.
        assert!(
            !unresolved.contains(&"externalValue".to_string()),
            "value binding `externalValue` referenced via typeof query must resolve: {unresolved:?}",
        );
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
        let tail = "init();\nprocess.env.FLAG = '1';\nmain();\n";
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
        assert_eq!(
            entrypoint
                .side_effects
                .iter()
                .map(|side_effect| side_effect.source.as_str())
                .collect::<Vec<_>>(),
            vec!["init();", "process.env.FLAG = '1';"]
        );
        assert!(source.contains("function main()"));
        assert!(source.contains("var helper"));
        assert!(source.contains("var dependency"));
        assert!(!source.contains("unused"));
        assert!(!source.contains("Missing"));
    }

    #[test]
    fn graph_records_runtime_namespace_exports_as_facts() {
        let prelude = concat!(
            "function expose(target, exports) {}\n",
            "var ns = {};\n",
            "expose(ns, { ready: () => ready, 'other-name': () => other });\n",
            "function ready() { return true; }\n",
            "function other() { return false; }\n",
        );
        let body = "export const moduleValue = ns;\n";
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
        let namespace_export = prelude
            .namespace_exports
            .iter()
            .find(|export| export.namespace.as_str() == "ns")
            .expect("namespace export should be recovered");
        let ns = BindingName::new("ns");
        let source = prelude.source_for_bindings(std::iter::once(&ns));

        assert_eq!(namespace_export.helper.as_str(), "expose");
        assert_eq!(namespace_export.exports["ready"].as_str(), "ready");
        assert_eq!(namespace_export.exports["other-name"].as_str(), "other");
        assert!(
            prelude.entrypoint.is_none(),
            "namespace initializer calls must not become runtime entrypoints"
        );
        assert!(source.contains("var ns = {};"));
        assert!(source.contains("function ready()"));
        assert!(source.contains("function other()"));
        assert!(!source.contains("function expose"));
        assert!(!source.contains("expose(ns"));
    }

    #[test]
    fn graph_records_object_define_property_runtime_namespace_exports() {
        let prelude = concat!(
            "var ns = {};\n",
            "var singleNs = {};\n",
            "Object.defineProperties(ns, {\n",
            "  ready: { enumerable: true, get: () => ready },\n",
            "  'other-name': { enumerable: true, get: function() { return other; } }\n",
            "});\n",
            "Object.defineProperty(singleNs, 'single', { enumerable: true, get: () => single });\n",
            "function ready() { return true; }\n",
            "function other() { return false; }\n",
            "function single() { return 1; }\n",
        );
        let body = "export const moduleValue = ns.ready + singleNs.single;\n";
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
        let ns_export = prelude
            .namespace_exports
            .iter()
            .find(|export| export.namespace.as_str() == "ns")
            .expect("Object.defineProperties namespace export should be recovered");
        let single_export = prelude
            .namespace_exports
            .iter()
            .find(|export| export.namespace.as_str() == "singleNs")
            .expect("Object.defineProperty namespace export should be recovered");

        assert_eq!(ns_export.helper.as_str(), "Object.defineProperties");
        assert_eq!(ns_export.exports["ready"].as_str(), "ready");
        assert_eq!(ns_export.exports["other-name"].as_str(), "other");
        assert_eq!(single_export.helper.as_str(), "Object.defineProperty");
        assert_eq!(single_export.exports["single"].as_str(), "single");
        assert!(
            prelude.entrypoint.is_none(),
            "Object.defineProperties calls must not become runtime entrypoints"
        );
    }

    #[test]
    fn runtime_snippet_dependencies_include_template_interpolations() {
        let prelude = concat!(
            "var dependency = 'value';\n",
            "function main() { return new RegExp(`^${dependency}$`); }\n",
        );
        let body = "export const moduleValue = 1;\n";
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
        let main = BindingName::new("main");
        let source = prelude.source_for_bindings(std::iter::once(&main));

        assert!(source.contains("function main()"));
        assert!(source.contains("var dependency = 'value';"));
    }

    #[test]
    fn runtime_snippet_dependencies_include_contextual_identifier_as() {
        let prelude = concat!(
            "var as = { command() { return this; } };\n",
            "function main() { return as.command('run'); }\n",
        );
        let body = "export const moduleValue = 1;\n";
        let source = format!("{prelude}{body}");
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
        let main = BindingName::new("main");
        let source = prelude.source_for_bindings(std::iter::once(&main));

        assert!(source.contains("function main()"));
        assert!(source.contains("var as = { command() { return this; } };"));
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

#[cfg(test)]
mod iife_kind_visibility_tests {
    use super::{AstWrapperKind, iife_kind};
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    #[test]
    fn iife_kind_is_public_and_recognises_function_iife() {
        let alloc = Allocator::default();
        let parsed = Parser::new(
            &alloc,
            "(function () { return 1; })()",
            SourceType::default(),
        )
        .parse();
        assert!(parsed.errors.is_empty());
        let stmt = parsed.program.body.first().expect("at least one statement");
        let oxc_ast::ast::Statement::ExpressionStatement(expr) = stmt else {
            panic!("expected expression statement");
        };
        let oxc_ast::ast::Expression::CallExpression(call) = &expr.expression else {
            panic!("expected call expression");
        };
        let kind = iife_kind(&call.callee);
        assert_eq!(kind, Some(AstWrapperKind::FunctionIife));
    }
}
