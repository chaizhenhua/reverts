use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::{
    Visit,
    ast::{
        Argument, ArrowFunctionExpression, AssignmentExpression, AssignmentTarget,
        BindingPatternKind, BlockStatement, CallExpression, ClassElement, Declaration,
        ExportAllDeclaration, ExportDefaultDeclarationKind, ExportNamedDeclaration, Expression,
        Function, FunctionBody, IdentifierReference, ImportDeclaration, ImportDeclarationSpecifier,
        ImportExpression, ImportOrExportKind, ModuleExportName, NewExpression, Program,
        SimpleAssignmentTarget, Statement, StringLiteral, UpdateExpression,
    },
    visit::walk::{
        walk_arrow_function_expression, walk_assignment_expression, walk_block_statement,
        walk_call_expression, walk_export_all_declaration, walk_export_named_declaration,
        walk_expression, walk_function, walk_function_body, walk_import_declaration,
        walk_import_expression, walk_new_expression, walk_program, walk_string_literal,
        walk_update_expression,
    },
};
use oxc_parser::Parser;
use oxc_semantic::{ScopeTree, SemanticBuilder, SymbolTable};
use oxc_span::GetSpan;
use oxc_syntax::operator::{LogicalOperator, UnaryOperator};
use oxc_syntax::scope::ScopeFlags;
use oxc_syntax::symbol::SymbolFlags;

use crate::errors::{JsError, ParseError, ParseGoal, Result};
use crate::parse::{parse_options_for, source_type_candidates};
use crate::{expression_identifier, module_export_name_text};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringLiteralFact {
    pub value: String,
    pub byte_start: u32,
    pub byte_end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticModuleSpecifierFact {
    pub value: String,
    pub byte_start: u32,
    pub byte_end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticTemplateLiteralFact {
    pub value: String,
    pub byte_start: u32,
    pub byte_end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementSpanFact {
    pub byte_start: u32,
    pub byte_end: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TopLevelStatementKind {
    Import,
    Export,
    Setter,
    Function,
    Class,
    LazyValue,
    LazyModule,
    Variable,
    Other,
}

impl TopLevelStatementKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Import => "import",
            Self::Export => "export",
            Self::Setter => "setter",
            Self::Function => "function",
            Self::Class => "class",
            Self::LazyValue => "lazy_value",
            Self::LazyModule => "lazy_module",
            Self::Variable => "variable",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopLevelStatementFact {
    pub kind: TopLevelStatementKind,
    pub bindings: Vec<String>,
    pub byte_start: u32,
    pub byte_end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocationRewriteFact {
    pub value: String,
    pub byte_start: u32,
    pub byte_end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathBuilderCallFact {
    pub string_arguments: Vec<String>,
    pub byte_start: u32,
    pub byte_end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentifierReadFact {
    pub name: String,
    pub byte_start: u32,
    pub byte_end: u32,
    pub is_call_callee: bool,
    pub call_arg_count: Option<u32>,
}

/// A statement-level slice of a `lazyValue(() => { ... })` body. Each
/// slice carries its source text and offsets relative to the surrounding
/// snippet, so the planner can evaluate migration of individual
/// statements (e.g. a single `class X = ...` inside the lazy body)
/// independently from the rest of the lazy block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LazyValueSubSnippet {
    pub source: String,
    /// Offset into the *enclosing* snippet's source text (not the global
    /// helper file).
    pub byte_start: u32,
    pub byte_end: u32,
    /// Top-level statement kind inside the lazy body.
    pub kind: TopLevelStatementKind,
    pub bindings: Vec<String>,
}

pub fn collect_string_literals(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<Vec<StringLiteralFact>> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }

        let mut collector = StringLiteralCollector::default();
        collector.visit_program(&parsed.program);
        return Ok(collector.literals);
    }

    Err(JsError::ParseFailed(errors))
}

pub fn collect_static_template_literals(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<Vec<StaticTemplateLiteralFact>> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }

        let mut collector = StaticTemplateLiteralCollector::default();
        collector.visit_program(&parsed.program);
        return Ok(collector.literals);
    }

    Err(JsError::ParseFailed(errors))
}

pub fn collect_void_zero_expression_statements(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<Vec<StatementSpanFact>> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }

        let mut collector = VoidZeroExpressionStatementCollector::default();
        collector.visit_program(&parsed.program);
        return Ok(collector.statements);
    }

    Err(JsError::ParseFailed(errors))
}

/// Try to slice `source` (the body of one `RuntimePreludeSnippet`) into
/// per-statement sub-snippets, when the snippet matches the
/// `var X = lazyValue(() => { ... });` (or `var X = lazyModule(...)`)
/// shape. Returns `None` for any other shape so callers fall back to the
/// whole snippet as one unit.
pub fn lazy_value_sub_snippets(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Option<Vec<LazyValueSubSnippet>> {
    let allocator = Allocator::default();
    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            continue;
        }
        // Only one top-level statement expected (a single `var X = lazyValue(...);`).
        let [Statement::VariableDeclaration(declaration)] = parsed.program.body.as_slice() else {
            return None;
        };
        let [declarator] = declaration.declarations.as_slice() else {
            return None;
        };
        let Some(Expression::CallExpression(call)) = declarator.init.as_ref() else {
            return None;
        };
        let callee = expression_identifier(&call.callee)?;
        if callee != "lazyValue" && callee != "lazyModule" {
            return None;
        }
        // The single argument must be an arrow function (`() => { BODY }`).
        let [argument] = call.arguments.as_slice() else {
            return None;
        };
        let Argument::ArrowFunctionExpression(arrow) = argument else {
            return None;
        };
        // Arrow body must be a function-body block; expression-only
        // arrows (`() => 1`) are single-statement lazies that aren't
        // worth slicing.
        if arrow.expression || arrow.body.statements.is_empty() {
            return None;
        }
        let memoizers = lazy_memoizer_names(&arrow.body.statements);
        let slices = arrow
            .body
            .statements
            .iter()
            .map(|statement| {
                let span = statement.span();
                let (kind, bindings) = top_level_statement_kind_and_bindings(statement, &memoizers);
                LazyValueSubSnippet {
                    source: source
                        .get(span.start as usize..span.end as usize)
                        .unwrap_or("")
                        .to_string(),
                    byte_start: span.start,
                    byte_end: span.end,
                    kind,
                    bindings,
                }
            })
            .collect();
        return Some(slices);
    }
    None
}

pub fn collect_top_level_statement_facts(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<Vec<TopLevelStatementFact>> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }

        let memoizers = lazy_memoizer_names(&parsed.program.body);
        return Ok(parsed
            .program
            .body
            .iter()
            .map(|statement| top_level_statement_fact(statement, &memoizers))
            .collect());
    }

    Err(JsError::ParseFailed(errors))
}

/// Local binding names the module exports, parsed from `source`. Same
/// definition as the dead-binding pass uses internally: declaration-form
/// exports, local named specifiers, and named default declarations; re-exports
/// bind no local symbol and are excluded.
pub fn collect_exported_top_level_bindings(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<BTreeSet<String>> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }
        return Ok(collect_exported_local_names(&parsed.program));
    }

    Err(JsError::ParseFailed(errors))
}

/// Module-scope binding names that are dead in the emitted module: declared but
/// never read or written, and not exported. These are esbuild's vestigial
/// hoists (`var A, i, n, o, s;` shadowed by function-local `let`s) and unused
/// unexported constants — they carry no semantic role and should not enter the
/// naming worklist.
///
/// Implemented via oxc's resolved references (the same read/write signal as
/// `ResolvedSymbolGraph::unread_bindings`): a root-scope symbol with zero
/// resolved references and no export is dead. Exported bindings are kept (they
/// are API surface even when unused inside the module); imports are skipped.
pub fn collect_dead_top_level_bindings(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<BTreeSet<String>> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }

        let exported = collect_exported_local_names(&parsed.program);
        let semantic = SemanticBuilder::new().build(&parsed.program).semantic;
        let symbols = semantic.symbols();
        let scopes = semantic.scopes();
        let mut dead = BTreeSet::new();
        for symbol_id in symbols.symbol_ids() {
            // Module (root) scope only — a root scope has no parent.
            if scopes
                .get_parent_id(symbols.get_scope_id(symbol_id))
                .is_some()
            {
                continue;
            }
            // Imports are not naming targets; never treat them as dead bindings.
            if symbols
                .get_flags(symbol_id)
                .intersects(SymbolFlags::Import | SymbolFlags::TypeImport)
            {
                continue;
            }
            let name = symbols.get_name(symbol_id);
            if exported.contains(name) {
                continue;
            }
            // Zero resolved references == never read or written anywhere.
            if symbols.get_resolved_references(symbol_id).next().is_none() {
                dead.insert(name.to_string());
            }
        }
        return Ok(dead);
    }

    Err(JsError::ParseFailed(errors))
}

/// Names of top-level function declarations whose body assigns to a *free*
/// variable that is itself a module-scope (root) binding — i.e. the function
/// mutates shared module state.
///
/// Such a function is NOT safe to relocate into a separate cluster module: an
/// ESM import binding is read-only, so the write would either throw or — once
/// the cluster file is rewritten by `localize_written_imports` — fork a private
/// local copy of the variable, severing it from the entry's copy that the rest
/// of the program reads and writes. The function must stay co-located with the
/// state it mutates.
///
/// Resolution is scope-accurate (oxc semantic): a write counts only when it
/// resolves to a root-scope symbol other than the function itself. Writes to a
/// closure variable declared inside the same top-level function (which would
/// move with it) and writes to unresolved globals (handled by implicit-global
/// pinning) do not count.
pub fn top_level_functions_writing_module_state(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<BTreeSet<String>> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }
        let semantic = SemanticBuilder::new().build(&parsed.program).semantic;
        let mut collector = ModuleStateWriteCollector {
            fn_depth: 0,
            current_top: None,
            symbols: semantic.symbols(),
            scopes: semantic.scopes(),
            pinned: BTreeSet::new(),
        };
        collector.visit_program(&parsed.program);
        return Ok(collector.pinned);
    }

    Err(JsError::ParseFailed(errors))
}

struct ModuleStateWriteCollector<'s> {
    /// Enclosing function/arrow nesting depth. `0` means module top level.
    fn_depth: u32,
    /// Name of the outermost top-level function declaration currently being
    /// visited (the one a write would be attributed to), if any.
    current_top: Option<String>,
    symbols: &'s SymbolTable,
    scopes: &'s ScopeTree,
    pinned: BTreeSet<String>,
}

impl<'a> Visit<'a> for ModuleStateWriteCollector<'_> {
    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        if self.fn_depth == 0 {
            self.current_top = it.id.as_ref().map(|id| id.name.as_str().to_string());
        }
        self.fn_depth += 1;
        walk_function(self, it, flags);
        self.fn_depth -= 1;
        if self.fn_depth == 0 {
            self.current_top = None;
        }
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        // A top-level arrow has no declaration name to pin; just track depth so
        // writes inside it are not misattributed to an outer function.
        self.fn_depth += 1;
        walk_arrow_function_expression(self, it);
        self.fn_depth -= 1;
        if self.fn_depth == 0 {
            self.current_top = None;
        }
    }

    fn visit_identifier_reference(&mut self, identifier: &IdentifierReference<'a>) {
        let Some(top) = self.current_top.as_ref() else {
            return;
        };
        let reference = self.symbols.get_reference(identifier.reference_id());
        if !reference.is_write() {
            return;
        }
        let Some(symbol_id) = reference.symbol_id() else {
            return;
        };
        // A module-scope (root) target other than the function itself is shared
        // state the function mutates in place.
        let scope_id = self.symbols.get_scope_id(symbol_id);
        if self.scopes.get_parent_id(scope_id).is_none()
            && self.symbols.get_name(symbol_id) != top.as_str()
        {
            self.pinned.insert(top.clone());
        }
    }
}

/// Local binding names that the module exports (declaration form
/// `export var/function/class X`, named specifiers `export { local }` without a
/// `from` source, and named default declarations). Re-exports
/// (`export { x } from './m'`) bind no local symbol and are excluded.
fn collect_exported_local_names(program: &Program<'_>) -> BTreeSet<String> {
    let mut exported = BTreeSet::new();
    for statement in &program.body {
        match statement {
            Statement::ExportNamedDeclaration(export) => {
                if let Some(declaration) = &export.declaration {
                    exported.extend(export_declaration_binding_names(declaration));
                }
                if export.source.is_none() {
                    for specifier in &export.specifiers {
                        if let Some(local) = module_export_local_name(&specifier.local) {
                            exported.insert(local.to_string());
                        }
                    }
                }
            }
            Statement::ExportDefaultDeclaration(export) => match &export.declaration {
                ExportDefaultDeclarationKind::FunctionDeclaration(function) => {
                    if let Some(id) = &function.id {
                        exported.insert(id.name.as_str().to_string());
                    }
                }
                ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                    if let Some(id) = &class.id {
                        exported.insert(id.name.as_str().to_string());
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    exported
}

/// One `import { a, b as c } from '<specifier>'` edge: the source specifier and
/// the PUBLIC (target-side) names it pulls — `a`/`b`, the wire names the target
/// module must export. The importer's own locals (`c`) are irrelevant to export
/// matching, so only the `imported` side is recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedImportEdge {
    pub specifier: String,
    pub imported_names: Vec<String>,
}

/// A module's cross-module named-import edges plus the public surface it
/// exports, parsed from one emitted file. The audit pairs each importer's
/// `imported_names` against the resolved target's `exported_names` to catch a
/// dangling wire name (`No matching export`) before esbuild does.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ModuleImportExportSurface {
    /// Every `import { … } from '…'` carrying at least one value named import.
    pub named_imports: Vec<NamedImportEdge>,
    /// Public names a consumer may import: declaration names, `export { … }`
    /// (and `export { x as y } from '…'`) exported names, `export default`
    /// (as `"default"`), and `export * as ns from '…'` (as `ns`).
    pub exported_names: BTreeSet<String>,
    /// A bare `export * from '…'` re-exports an opaque set, so an unlisted
    /// imported name may still resolve through it. Verification is unsound for
    /// such a target, so the audit treats it as exporting anything.
    pub has_export_star: bool,
}

/// Parse `source` and collect its named-import edges and public export surface
/// (see [`ModuleImportExportSurface`]). Only top-level statements are scanned —
/// emitted modules carry imports/exports at module scope. Type-only imports and
/// exports are ignored: they bind no runtime value, so they neither demand nor
/// satisfy a runtime wire name.
pub fn collect_module_import_export_surface(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<ModuleImportExportSurface> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }
        return Ok(collect_import_export_surface(&parsed.program));
    }

    Err(JsError::ParseFailed(errors))
}

fn collect_import_export_surface(program: &Program<'_>) -> ModuleImportExportSurface {
    let mut surface = ModuleImportExportSurface::default();
    for statement in &program.body {
        match statement {
            Statement::ImportDeclaration(import) => {
                // A type-only `import type { … }` binds nothing at runtime.
                if import.import_kind == ImportOrExportKind::Type {
                    continue;
                }
                let Some(specifiers) = &import.specifiers else {
                    continue;
                };
                let mut imported_names = Vec::new();
                for specifier in specifiers {
                    // Only `{ name }` / `{ name as local }`; namespace and
                    // default specifiers don't demand a specific wire name.
                    if let ImportDeclarationSpecifier::ImportSpecifier(named) = specifier {
                        if named.import_kind == ImportOrExportKind::Type {
                            continue;
                        }
                        if let Some(name) = module_export_local_name(&named.imported) {
                            imported_names.push(name.to_string());
                        }
                    }
                }
                if !imported_names.is_empty() {
                    surface.named_imports.push(NamedImportEdge {
                        specifier: import.source.value.as_str().to_string(),
                        imported_names,
                    });
                }
            }
            Statement::ExportNamedDeclaration(export) => {
                if export.export_kind == ImportOrExportKind::Type {
                    continue;
                }
                if let Some(declaration) = &export.declaration {
                    surface
                        .exported_names
                        .extend(export_declaration_binding_names(declaration));
                }
                for specifier in &export.specifiers {
                    if specifier.export_kind == ImportOrExportKind::Type {
                        continue;
                    }
                    // The PUBLIC name is the `exported` side, for both
                    // `export { x }` and `export { local as Public } [from '…']`.
                    if let Some(name) = module_export_local_name(&specifier.exported) {
                        surface.exported_names.insert(name.to_string());
                    }
                }
            }
            Statement::ExportDefaultDeclaration(_) => {
                surface.exported_names.insert("default".to_string());
            }
            Statement::ExportAllDeclaration(export) => match &export.exported {
                Some(name) => {
                    if let Some(text) = module_export_local_name(name) {
                        surface.exported_names.insert(text.to_string());
                    }
                }
                None => surface.has_export_star = true,
            },
            _ => {}
        }
    }
    surface
}

fn module_export_local_name<'a>(name: &'a ModuleExportName<'a>) -> Option<&'a str> {
    match name {
        ModuleExportName::IdentifierName(identifier) => Some(identifier.name.as_str()),
        ModuleExportName::IdentifierReference(identifier) => Some(identifier.name.as_str()),
        ModuleExportName::StringLiteral(_) => None,
    }
}

pub fn collect_static_resource_specifiers(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<Vec<StringLiteralFact>> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }

        let mut collector = StaticResourceSpecifierCollector::default();
        collector.visit_program(&parsed.program);
        return Ok(collector.specifiers);
    }

    Err(JsError::ParseFailed(errors))
}

pub fn collect_static_module_specifiers(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<Vec<StaticModuleSpecifierFact>> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }

        let mut collector = StaticModuleSpecifierCollector::default();
        collector.visit_program(&parsed.program);
        return Ok(collector.specifiers);
    }

    Err(JsError::ParseFailed(errors))
}

pub fn collect_file_url_source_location_rewrites(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<Vec<SourceLocationRewriteFact>> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }

        let mut collector = FileUrlSourceLocationCollector::default();
        collector.visit_program(&parsed.program);
        return Ok(collector.rewrites);
    }

    Err(JsError::ParseFailed(errors))
}

pub fn collect_path_builder_calls(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<Vec<PathBuilderCallFact>> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }

        let mut collector = PathBuilderCallCollector::default();
        collector.visit_program(&parsed.program);
        return Ok(collector.calls);
    }

    Err(JsError::ParseFailed(errors))
}

pub fn collect_identifier_read_facts(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<Vec<IdentifierReadFact>> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }

        let mut collector = IdentifierReadCollector::default();
        collector.visit_program(&parsed.program);
        return Ok(collector.into_facts());
    }

    Err(JsError::ParseFailed(errors))
}

/// The names referenced but not bound at module scope — the source's free
/// variables (external references plus runtime globals). Scope-aware via OXC
/// semantic resolution: every identifier resolved to a binding, however deeply
/// nested, is excluded. Unlike a flat lexical scan this never mistakes a
/// function-local for a free reference, so callers can resolve exactly the
/// cross-module references a relocated fragment depends on.
pub fn free_identifiers_in_source(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<BTreeSet<String>> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }

        let semantic = SemanticBuilder::new().build(&parsed.program).semantic;
        return Ok(semantic
            .scopes()
            .root_unresolved_references()
            .keys()
            .map(|name| name.as_str().to_string())
            .collect());
    }

    Err(JsError::ParseFailed(errors))
}

/// A top-level class and the identifiers it references at DEFINITION time — the
/// positions JS evaluates when the `class` statement executes: the `extends`
/// clause, decorators, computed member keys, `static` field initializers, and
/// `static {}` blocks. Instance fields, method bodies, and the constructor are
/// excluded — they run later (construction / call), not at definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassEagerReferences {
    pub class_name: String,
    pub references: BTreeSet<String>,
}

/// For each top-level `class` declaration, collect its definition-time
/// references (see [`ClassEagerReferences`]). A relocation pass uses this to
/// decide whether moving a class across the entry↔cluster import boundary is
/// eval-order-safe: it is safe only when none of these references is an eager
/// binding that initializes after the moved class's module loads.
pub fn collect_top_level_class_eager_references(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<Vec<ClassEagerReferences>> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }

        let mut out = Vec::new();
        for statement in &parsed.program.body {
            let Statement::ClassDeclaration(class) = statement else {
                continue;
            };
            let Some(id) = class.id.as_ref() else {
                continue;
            };
            let mut collector = NameReferenceCollector::default();
            if let Some(super_class) = &class.super_class {
                collector.visit_expression(super_class);
            }
            for decorator in &class.decorators {
                collector.visit_expression(&decorator.expression);
            }
            for element in &class.body.body {
                match element {
                    ClassElement::PropertyDefinition(property) => {
                        for decorator in &property.decorators {
                            collector.visit_expression(&decorator.expression);
                        }
                        if property.computed
                            && let Some(key) = property.key.as_expression()
                        {
                            collector.visit_expression(key);
                        }
                        // Only STATIC field initializers run at definition time.
                        if property.r#static
                            && let Some(value) = &property.value
                        {
                            collector.visit_expression(value);
                        }
                    }
                    ClassElement::AccessorProperty(accessor) => {
                        for decorator in &accessor.decorators {
                            collector.visit_expression(&decorator.expression);
                        }
                        if accessor.computed
                            && let Some(key) = accessor.key.as_expression()
                        {
                            collector.visit_expression(key);
                        }
                        if accessor.r#static
                            && let Some(value) = &accessor.value
                        {
                            collector.visit_expression(value);
                        }
                    }
                    ClassElement::MethodDefinition(method) => {
                        for decorator in &method.decorators {
                            collector.visit_expression(&decorator.expression);
                        }
                        // A computed key is evaluated at definition time; the
                        // method body is inert until called, so it is skipped.
                        if method.computed
                            && let Some(key) = method.key.as_expression()
                        {
                            collector.visit_expression(key);
                        }
                    }
                    ClassElement::StaticBlock(block) => {
                        for statement in &block.body {
                            collector.visit_statement(statement);
                        }
                    }
                    ClassElement::TSIndexSignature(_) => {}
                }
            }
            out.push(ClassEagerReferences {
                class_name: id.name.to_string(),
                references: collector.names,
            });
        }
        return Ok(out);
    }

    Err(JsError::ParseFailed(errors))
}

/// Names assigned or updated anywhere in `source` as a plain identifier target
/// (`x = …`, `x += …`, `x++`, `[x] = …` are covered via the identifier target;
/// member writes like `x.y = …` are not — those mutate, they do not rebind). A
/// binding that appears here cannot be relocated into an imported module, since
/// ES imports are read-only.
pub fn collect_reassigned_binding_names(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<BTreeSet<String>> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }
        let mut collector = ReassignedBindingCollector::default();
        collector.visit_program(&parsed.program);
        return Ok(collector.names);
    }

    Err(JsError::ParseFailed(errors))
}

#[derive(Debug, Default)]
struct ReassignedBindingCollector {
    names: BTreeSet<String>,
}

impl<'a> Visit<'a> for ReassignedBindingCollector {
    fn visit_assignment_expression(&mut self, expression: &AssignmentExpression<'a>) {
        if let AssignmentTarget::AssignmentTargetIdentifier(identifier) = &expression.left {
            self.names.insert(identifier.name.as_str().to_string());
        }
        walk_assignment_expression(self, expression);
    }
    fn visit_update_expression(&mut self, expression: &UpdateExpression<'a>) {
        if let SimpleAssignmentTarget::AssignmentTargetIdentifier(identifier) = &expression.argument
        {
            self.names.insert(identifier.name.as_str().to_string());
        }
        walk_update_expression(self, expression);
    }
}

/// A top-level `var`/`const`/`let` binding and what its initializer does AT
/// EVALUATION time (when the declaration statement runs). `pure` is false if the
/// eagerly-evaluated part performs an observable operation (call, `new`, `await`,
/// assignment, update, tagged template) — such an initializer cannot be hoisted
/// to a before-entry module without risking changed behavior. `eager_references`
/// are the identifiers read at evaluation time; references inside the
/// initializer's own function/arrow bodies are excluded (inert until called).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EagerBindingFact {
    pub name: String,
    pub pure: bool,
    pub eager_references: BTreeSet<String>,
}

/// For each top-level single-identifier `var`/`const`/`let` declarator with an
/// initializer, report its evaluation-time purity and references (see
/// [`EagerBindingFact`]). Destructuring and initializer-less declarators are
/// skipped (the relocation gate treats anything absent here as immovable).
pub fn collect_top_level_eager_bindings(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<Vec<EagerBindingFact>> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }

        let mut out = Vec::new();
        for statement in &parsed.program.body {
            let Statement::VariableDeclaration(declaration) = statement else {
                continue;
            };
            for declarator in &declaration.declarations {
                let Some(name) = declarator.id.get_identifier() else {
                    continue; // destructuring pattern → not handled
                };
                let Some(init) = &declarator.init else {
                    continue; // no initializer → nothing to evaluate
                };
                let mut collector = EagerInitCollector::default();
                collector.visit_expression(init);
                out.push(EagerBindingFact {
                    name: name.as_str().to_string(),
                    pure: !collector.impure,
                    eager_references: collector.references,
                });
            }
        }
        return Ok(out);
    }

    Err(JsError::ParseFailed(errors))
}

/// Walks an initializer's EAGERLY-evaluated part: never descends into
/// function/arrow bodies (inert until called) and flags any observable operation
/// as impure, collecting the identifier references evaluated at declaration time.
#[derive(Debug, Default)]
struct EagerInitCollector {
    references: BTreeSet<String>,
    impure: bool,
}

impl<'a> Visit<'a> for EagerInitCollector {
    fn visit_identifier_reference(&mut self, identifier: &IdentifierReference<'a>) {
        self.references.insert(identifier.name.to_string());
    }
    fn visit_call_expression(&mut self, _call: &CallExpression<'a>) {
        self.impure = true; // eager call: side effect / order dependence
    }
    fn visit_new_expression(&mut self, _expression: &NewExpression<'a>) {
        self.impure = true;
    }
    fn visit_await_expression(&mut self, _expression: &oxc_ast::ast::AwaitExpression<'a>) {
        self.impure = true;
    }
    fn visit_assignment_expression(
        &mut self,
        _expression: &oxc_ast::ast::AssignmentExpression<'a>,
    ) {
        self.impure = true;
    }
    fn visit_update_expression(&mut self, _expression: &oxc_ast::ast::UpdateExpression<'a>) {
        self.impure = true;
    }
    fn visit_tagged_template_expression(
        &mut self,
        _expression: &oxc_ast::ast::TaggedTemplateExpression<'a>,
    ) {
        self.impure = true; // the tag is called eagerly
    }
    // Function and arrow bodies are inert until called — do not descend.
    fn visit_function(&mut self, _function: &Function<'a>, _flags: ScopeFlags) {}
    fn visit_arrow_function_expression(&mut self, _arrow: &ArrowFunctionExpression<'a>) {}
}

/// Collects the names of every identifier REFERENCE reached while visiting,
/// used to read the free-ish references inside a class's definition-time nodes.
/// Over-approximates (e.g. counts references inside a static initializer's
/// nested arrow body) — safe for the relocation gate, which only ever keeps a
/// class inline when in doubt.
#[derive(Debug, Default)]
struct NameReferenceCollector {
    names: BTreeSet<String>,
}

impl<'a> Visit<'a> for NameReferenceCollector {
    fn visit_identifier_reference(&mut self, identifier: &IdentifierReference<'a>) {
        self.names.insert(identifier.name.to_string());
    }
}

#[derive(Debug, Default)]
struct StringLiteralCollector {
    literals: Vec<StringLiteralFact>,
}

impl<'a> Visit<'a> for StringLiteralCollector {
    fn visit_string_literal(&mut self, literal: &StringLiteral<'a>) {
        self.literals.push(StringLiteralFact {
            value: literal.value.as_str().to_string(),
            byte_start: literal.span.start,
            byte_end: literal.span.end,
        });
        walk_string_literal(self, literal);
    }
}

#[derive(Debug, Default)]
struct StaticTemplateLiteralCollector {
    literals: Vec<StaticTemplateLiteralFact>,
}

impl<'a> Visit<'a> for StaticTemplateLiteralCollector {
    fn visit_expression(&mut self, expression: &Expression<'a>) {
        if let Expression::TemplateLiteral(template) = expression
            && template.expressions.is_empty()
            && let Some(quasi) = template.quasis.first()
            && let Some(value) = quasi.value.cooked.as_deref()
        {
            let span = expression.span();
            self.literals.push(StaticTemplateLiteralFact {
                value: value.to_string(),
                byte_start: span.start,
                byte_end: span.end,
            });
        }
        walk_expression(self, expression);
    }
}

#[derive(Debug, Default)]
struct VoidZeroExpressionStatementCollector {
    statements: Vec<StatementSpanFact>,
}

impl<'a> Visit<'a> for VoidZeroExpressionStatementCollector {
    fn visit_program(&mut self, program: &Program<'a>) {
        self.collect_statement_list(&program.body);
        walk_program(self, program);
    }

    fn visit_function_body(&mut self, body: &FunctionBody<'a>) {
        self.collect_statement_list(&body.statements);
        walk_function_body(self, body);
    }

    fn visit_block_statement(&mut self, block: &BlockStatement<'a>) {
        self.collect_statement_list(&block.body);
        walk_block_statement(self, block);
    }

    fn visit_arrow_function_expression(&mut self, arrow: &ArrowFunctionExpression<'a>) {
        // A concise-body arrow (`() => void 0`) is modelled by OXC as a
        // FunctionBody holding one synthetic ExpressionStatement wrapping the
        // returned expression. That statement is NOT erasable: removing a
        // `void 0` concise body would leave `() =>`, which is invalid TS and
        // panics the emitter's parse-audit. Walk into the body so genuinely
        // nested erasable statements are still found, but do not collect the
        // concise body's own statement. Block-body arrows collect normally.
        if arrow.expression {
            walk_function_body(self, &arrow.body);
        } else {
            self.visit_function_body(&arrow.body);
        }
    }
}

impl VoidZeroExpressionStatementCollector {
    fn collect_statement_list<'a>(&mut self, statements: &[Statement<'a>]) {
        self.statements
            .extend(statements.iter().filter_map(void_zero_statement_span));
    }
}

fn void_zero_statement_span(statement: &Statement<'_>) -> Option<StatementSpanFact> {
    let Statement::ExpressionStatement(expression_statement) = statement else {
        return None;
    };
    if !matches_void_numeric_expression(&expression_statement.expression) {
        return None;
    }
    let span = statement.span();
    Some(StatementSpanFact {
        byte_start: span.start,
        byte_end: span.end,
    })
}

fn matches_void_numeric_expression(expression: &Expression<'_>) -> bool {
    let Expression::UnaryExpression(unary) = expression else {
        return false;
    };
    matches!(unary.operator, UnaryOperator::Void)
        && matches!(&unary.argument, Expression::NumericLiteral(_))
}

pub(crate) fn top_level_statement_fact(
    statement: &Statement<'_>,
    memoizers: &BTreeSet<String>,
) -> TopLevelStatementFact {
    let span = statement.span();
    let (kind, bindings) = top_level_statement_kind_and_bindings(statement, memoizers);
    TopLevelStatementFact {
        kind,
        bindings,
        byte_start: span.start,
        byte_end: span.end,
    }
}

/// True if `expr` is the esbuild `__esm` memoizer helper by structural
/// signature: `(a, b) => () => (a && (b = a(a = 0)), b)`. Name-independent — the
/// var it is bound to and both params may carry any (minified) identifiers. This
/// lets the classifier tag the planner's inlined per-file memoizer (`_$l`) the
/// same as the imported `lazyValue`, without hard-coding the name.
fn strip_parens<'a>(expr: &'a Expression<'a>) -> &'a Expression<'a> {
    match expr {
        Expression::ParenthesizedExpression(inner) => strip_parens(&inner.expression),
        other => other,
    }
}

fn is_lazy_memoizer_init(expr: &Expression<'_>) -> bool {
    let Expression::ArrowFunctionExpression(outer) = strip_parens(expr) else {
        return false;
    };
    let params: Vec<&str> = outer
        .params
        .items
        .iter()
        .filter_map(|param| match &param.pattern.kind {
            BindingPatternKind::BindingIdentifier(id) => Some(id.name.as_str()),
            _ => None,
        })
        .collect();
    let [a, b] = params.as_slice() else {
        return false;
    };
    let Some(Expression::ArrowFunctionExpression(inner)) = outer.get_expression().map(strip_parens)
    else {
        return false;
    };
    if !inner.params.items.is_empty() {
        return false;
    }
    let Some(Expression::SequenceExpression(seq)) = inner.get_expression().map(strip_parens) else {
        return false;
    };
    let [first, last] = seq.expressions.as_slice() else {
        return false;
    };
    // last operand is the cached value identifier `b`
    let Expression::Identifier(last_id) = strip_parens(last) else {
        return false;
    };
    if last_id.name.as_str() != *b {
        return false;
    }
    // first operand is `a && (…)`
    let Expression::LogicalExpression(logical) = strip_parens(first) else {
        return false;
    };
    matches!(logical.operator, LogicalOperator::And)
        && matches!(strip_parens(&logical.left), Expression::Identifier(left) if left.name.as_str() == *a)
}

/// Names of file-local lazy memoizers, found by [`is_lazy_memoizer_init`]
/// signature over a program's top-level declarations.
pub(crate) fn lazy_memoizer_names(body: &[Statement<'_>]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for statement in body {
        let Statement::VariableDeclaration(declaration) = statement else {
            continue;
        };
        for declarator in &declaration.declarations {
            let (BindingPatternKind::BindingIdentifier(id), Some(init)) =
                (&declarator.id.kind, declarator.init.as_ref())
            else {
                continue;
            };
            if is_lazy_memoizer_init(init) {
                names.insert(id.name.as_str().to_string());
            }
        }
    }
    names
}

pub(crate) fn top_level_statement_kind_and_bindings(
    statement: &Statement<'_>,
    memoizers: &BTreeSet<String>,
) -> (TopLevelStatementKind, Vec<String>) {
    match statement {
        Statement::ImportDeclaration(_) => (TopLevelStatementKind::Import, Vec::new()),
        Statement::FunctionDeclaration(function) => {
            let bindings = function
                .id
                .as_ref()
                .map(|id| id.name.as_str().to_string())
                .into_iter()
                .collect::<Vec<_>>();
            let kind = if bindings
                .first()
                .is_some_and(|binding| binding.starts_with("__reverts_set_"))
            {
                TopLevelStatementKind::Setter
            } else {
                TopLevelStatementKind::Function
            };
            (kind, bindings)
        }
        Statement::ClassDeclaration(class) => (
            TopLevelStatementKind::Class,
            class
                .id
                .as_ref()
                .map(|id| id.name.as_str().to_string())
                .into_iter()
                .collect(),
        ),
        Statement::VariableDeclaration(declaration) => {
            let bindings = declaration_binding_names(declaration);
            let kind = declaration
                .declarations
                .iter()
                .find_map(|declarator| {
                    let init = declarator.init.as_ref()?;
                    let Expression::CallExpression(call) = init else {
                        return None;
                    };
                    match expression_identifier(&call.callee) {
                        Some("lazyValue") => Some(TopLevelStatementKind::LazyValue),
                        Some("lazyModule") => Some(TopLevelStatementKind::LazyModule),
                        // Inlined per-file memoizer (`_$l` etc.), recognized by
                        // signature rather than name.
                        Some(name) if memoizers.contains(name) => {
                            Some(TopLevelStatementKind::LazyValue)
                        }
                        _ => None,
                    }
                })
                .unwrap_or(TopLevelStatementKind::Variable);
            (kind, bindings)
        }
        Statement::ExportNamedDeclaration(export) => {
            let bindings = export
                .declaration
                .as_ref()
                .map(export_declaration_binding_names)
                .unwrap_or_default();
            (TopLevelStatementKind::Export, bindings)
        }
        Statement::ExportDefaultDeclaration(export) => {
            let bindings = match &export.declaration {
                ExportDefaultDeclarationKind::FunctionDeclaration(function) => function
                    .id
                    .as_ref()
                    .map(|id| id.name.as_str().to_string())
                    .into_iter()
                    .collect(),
                ExportDefaultDeclarationKind::ClassDeclaration(class) => class
                    .id
                    .as_ref()
                    .map(|id| id.name.as_str().to_string())
                    .into_iter()
                    .collect(),
                _ => Vec::new(),
            };
            (TopLevelStatementKind::Export, bindings)
        }
        Statement::ExportAllDeclaration(_) => (TopLevelStatementKind::Export, Vec::new()),
        _ => (TopLevelStatementKind::Other, Vec::new()),
    }
}

fn export_declaration_binding_names(declaration: &Declaration<'_>) -> Vec<String> {
    match declaration {
        Declaration::FunctionDeclaration(function) => function
            .id
            .as_ref()
            .map(|id| id.name.as_str().to_string())
            .into_iter()
            .collect(),
        Declaration::ClassDeclaration(class) => class
            .id
            .as_ref()
            .map(|id| id.name.as_str().to_string())
            .into_iter()
            .collect(),
        Declaration::VariableDeclaration(declaration) => declaration_binding_names(declaration),
        _ => Vec::new(),
    }
}

fn declaration_binding_names(declaration: &oxc_ast::ast::VariableDeclaration<'_>) -> Vec<String> {
    let mut names = Vec::new();
    for declarator in &declaration.declarations {
        collect_binding_pattern_names(&declarator.id, &mut names);
    }
    names
}

/// Collect every identifier a binding pattern introduces, recursing through
/// object/array destructuring, defaults, and rest elements. A simple
/// `BindingIdentifier`-only scan silently drops names bound by destructuring
/// (e.g. `var { isArray: rH } = Array` binds `rH`), which then look undefined to
/// any consumer that imports them across a module boundary.
fn collect_binding_pattern_names(
    pattern: &oxc_ast::ast::BindingPattern<'_>,
    names: &mut Vec<String>,
) {
    match &pattern.kind {
        BindingPatternKind::BindingIdentifier(identifier) => {
            names.push(identifier.name.as_str().to_string());
        }
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

#[derive(Debug, Default)]
struct IdentifierReadCollector {
    facts: BTreeMap<(u32, u32, String), IdentifierReadAccumulator>,
}

#[derive(Debug, Default)]
struct IdentifierReadAccumulator {
    is_call_callee: bool,
    call_arg_count: Option<u32>,
}

impl IdentifierReadCollector {
    fn push_identifier(
        &mut self,
        identifier: &IdentifierReference<'_>,
        is_call_callee: bool,
        call_arg_count: Option<u32>,
    ) {
        let span = identifier.span();
        let entry = self
            .facts
            .entry((span.start, span.end, identifier.name.as_str().to_string()))
            .or_default();
        entry.is_call_callee |= is_call_callee;
        if call_arg_count.is_some() {
            entry.call_arg_count = call_arg_count;
        }
    }

    fn into_facts(self) -> Vec<IdentifierReadFact> {
        self.facts
            .into_iter()
            .map(
                |((byte_start, byte_end, name), accumulator)| IdentifierReadFact {
                    name,
                    byte_start,
                    byte_end,
                    is_call_callee: accumulator.is_call_callee,
                    call_arg_count: accumulator.call_arg_count,
                },
            )
            .collect()
    }
}

impl<'a> Visit<'a> for IdentifierReadCollector {
    fn visit_export_named_declaration(&mut self, declaration: &ExportNamedDeclaration<'a>) {
        if declaration.source.is_none() {
            for specifier in &declaration.specifiers {
                if let Some(local) = module_export_name_text(&specifier.local) {
                    let span = specifier.local.span();
                    self.facts.entry((span.start, span.end, local)).or_default();
                }
            }
        }
        walk_export_named_declaration(self, declaration);
    }

    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if let Expression::Identifier(callee) = &call.callee {
            self.push_identifier(callee, true, Some(call.arguments.len() as u32));
        }
        walk_call_expression(self, call);
    }

    fn visit_new_expression(&mut self, expression: &NewExpression<'a>) {
        if let Expression::Identifier(callee) = &expression.callee {
            self.push_identifier(callee, true, None);
        }
        walk_new_expression(self, expression);
    }

    fn visit_identifier_reference(&mut self, identifier: &IdentifierReference<'a>) {
        self.push_identifier(identifier, false, None);
    }
}

#[derive(Debug, Default)]
struct StaticResourceSpecifierCollector {
    specifiers: Vec<StringLiteralFact>,
}

impl<'a> Visit<'a> for StaticResourceSpecifierCollector {
    fn visit_import_declaration(&mut self, declaration: &ImportDeclaration<'a>) {
        self.push_literal(&declaration.source);
        walk_import_declaration(self, declaration);
    }

    fn visit_export_named_declaration(&mut self, declaration: &ExportNamedDeclaration<'a>) {
        if let Some(source) = declaration.source.as_ref() {
            self.push_literal(source);
        }
        walk_export_named_declaration(self, declaration);
    }

    fn visit_export_all_declaration(&mut self, declaration: &ExportAllDeclaration<'a>) {
        self.push_literal(&declaration.source);
        walk_export_all_declaration(self, declaration);
    }

    fn visit_import_expression(&mut self, expression: &ImportExpression<'a>) {
        if let Expression::StringLiteral(source) = &expression.source {
            self.push_literal(source);
        }
        walk_import_expression(self, expression);
    }

    fn visit_call_expression(&mut self, expression: &CallExpression<'a>) {
        if call_callee_accepts_static_resource(&expression.callee)
            && let Some(Argument::StringLiteral(source)) = expression.arguments.first()
        {
            self.push_literal(source);
        }
        walk_call_expression(self, expression);
    }

    fn visit_new_expression(&mut self, expression: &NewExpression<'a>) {
        if expression_identifier(&expression.callee) == Some("URL")
            && let Some(Argument::StringLiteral(source)) = expression.arguments.first()
        {
            self.push_literal(source);
        }
        walk_new_expression(self, expression);
    }
}

impl StaticResourceSpecifierCollector {
    fn push_literal(&mut self, literal: &StringLiteral<'_>) {
        self.specifiers.push(StringLiteralFact {
            value: literal.value.as_str().to_string(),
            byte_start: literal.span.start,
            byte_end: literal.span.end,
        });
    }
}

#[derive(Debug, Default)]
struct StaticModuleSpecifierCollector {
    specifiers: Vec<StaticModuleSpecifierFact>,
}

impl<'a> Visit<'a> for StaticModuleSpecifierCollector {
    fn visit_import_declaration(&mut self, declaration: &ImportDeclaration<'a>) {
        self.push_literal(&declaration.source);
        walk_import_declaration(self, declaration);
    }

    fn visit_export_named_declaration(&mut self, declaration: &ExportNamedDeclaration<'a>) {
        if let Some(source) = declaration.source.as_ref() {
            self.push_literal(source);
        }
        walk_export_named_declaration(self, declaration);
    }

    fn visit_export_all_declaration(&mut self, declaration: &ExportAllDeclaration<'a>) {
        self.push_literal(&declaration.source);
        walk_export_all_declaration(self, declaration);
    }

    fn visit_import_expression(&mut self, expression: &ImportExpression<'a>) {
        if let Expression::StringLiteral(source) = &expression.source {
            self.push_literal(source);
        }
        walk_import_expression(self, expression);
    }

    fn visit_call_expression(&mut self, expression: &CallExpression<'a>) {
        if expression_identifier(&expression.callee) == Some("require")
            && let Some(Argument::StringLiteral(source)) = expression.arguments.first()
        {
            self.push_literal(source);
        }
        walk_call_expression(self, expression);
    }
}

impl StaticModuleSpecifierCollector {
    fn push_literal(&mut self, literal: &StringLiteral<'_>) {
        self.specifiers.push(StaticModuleSpecifierFact {
            value: literal.value.as_str().to_string(),
            byte_start: literal.span.start,
            byte_end: literal.span.end,
        });
    }
}

fn call_callee_accepts_static_resource(callee: &Expression<'_>) -> bool {
    if expression_identifier(callee) == Some("require") {
        return true;
    }

    let Expression::StaticMemberExpression(member) = callee else {
        return false;
    };
    let property = member.property.name.as_str();
    matches!(
        (expression_identifier(&member.object), property),
        (Some("require"), "resolve")
            | (Some("Bun"), "file")
            | (Some("fs"), "readFile")
            | (Some("fs"), "readFileSync")
            | (Some("fs"), "createReadStream")
    )
}

#[derive(Debug, Default)]
struct FileUrlSourceLocationCollector {
    rewrites: Vec<SourceLocationRewriteFact>,
}

impl<'a> Visit<'a> for FileUrlSourceLocationCollector {
    fn visit_call_expression(&mut self, expression: &CallExpression<'a>) {
        if call_callee_property_or_identifier(&expression.callee) == Some("fileURLToPath")
            && let Some(Argument::StringLiteral(source)) = expression.arguments.first()
            && is_file_url_source_location(source.value.as_str())
        {
            self.rewrites.push(SourceLocationRewriteFact {
                value: source.value.as_str().to_string(),
                byte_start: source.span.start,
                byte_end: source.span.end,
            });
        }
        walk_call_expression(self, expression);
    }
}

#[derive(Debug, Default)]
struct PathBuilderCallCollector {
    calls: Vec<PathBuilderCallFact>,
}

impl<'a> Visit<'a> for PathBuilderCallCollector {
    fn visit_call_expression(&mut self, expression: &CallExpression<'a>) {
        if matches!(
            call_callee_property_or_identifier(&expression.callee),
            Some("join" | "resolve")
        ) {
            let string_arguments = expression
                .arguments
                .iter()
                .filter_map(argument_string_literal_value)
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            if !string_arguments.is_empty() {
                self.calls.push(PathBuilderCallFact {
                    string_arguments,
                    byte_start: expression.span.start,
                    byte_end: expression.span.end,
                });
            }
        }
        walk_call_expression(self, expression);
    }
}

fn argument_string_literal_value<'a>(argument: &'a Argument<'a>) -> Option<&'a str> {
    match argument {
        Argument::StringLiteral(literal) => Some(literal.value.as_str()),
        _ => None,
    }
}

fn call_callee_property_or_identifier<'a>(callee: &'a Expression<'a>) -> Option<&'a str> {
    match callee {
        Expression::Identifier(identifier) => Some(identifier.name.as_str()),
        Expression::StaticMemberExpression(member) => Some(member.property.name.as_str()),
        _ => None,
    }
}

fn is_file_url_source_location(value: &str) -> bool {
    let Some(path) = value.strip_prefix("file://") else {
        return false;
    };
    let path = path.strip_prefix("localhost").unwrap_or(path);
    let extension = Path::new(path)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_ascii_lowercase);
    matches!(
        extension.as_deref(),
        Some("js" | "mjs" | "cjs" | "ts" | "mts" | "cts" | "jsx" | "tsx")
    )
}

#[cfg(test)]
mod void_zero_collector_tests {
    use super::{
        collect_static_module_specifiers, collect_top_level_statement_facts,
        collect_void_zero_expression_statements,
    };
    use crate::errors::ParseGoal;

    fn spans(source: &str) -> Vec<(u32, u32)> {
        collect_void_zero_expression_statements(source, None, ParseGoal::TypeScript)
            .expect("parseable")
            .into_iter()
            .map(|fact| (fact.byte_start, fact.byte_end))
            .collect()
    }

    #[test]
    fn concise_arrow_void_zero_body_is_not_collected() {
        // Erasing this `void 0` would leave `() =>` — invalid TS. The concise
        // arrow body must be left intact.
        let source = "Promise.resolve().then(() => void 0);";
        assert!(
            spans(source).is_empty(),
            "concise arrow body must not be treated as an erasable statement: {:?}",
            spans(source)
        );
    }

    #[test]
    fn real_bare_void_zero_statement_is_still_collected() {
        let source = "void 0;\nconst x = 1;";
        assert_eq!(spans(source).len(), 1, "a top-level `void 0;` is erasable");
    }

    #[test]
    fn block_arrow_body_void_zero_statement_is_collected() {
        // Inside a block body, `void 0;` is a genuine statement and erasing it
        // leaves a valid `() => {}`.
        let source = "const f = () => { void 0; };";
        assert_eq!(spans(source).len(), 1);
    }

    #[test]
    fn top_level_statement_facts_capture_destructuring_bindings() {
        // `var { isArray: rH } = Array` binds `rH`; array and rest/default
        // patterns bind their names too. A scan that only handled
        // `BindingIdentifier` dropped these, so a chunk owning such a binding
        // never exported it and cross-module importers dangled.
        let source = "var { isArray: rH, from: gA } = Array;\nconst [a, , b] = xs;\nlet { p = 1, ...rest } = o;";
        let facts = collect_top_level_statement_facts(source, None, ParseGoal::TypeScript)
            .expect("collect top-level statement facts");
        let bindings: std::collections::BTreeSet<&str> = facts
            .iter()
            .flat_map(|fact| fact.bindings.iter().map(String::as_str))
            .collect();
        for expected in ["rH", "gA", "a", "b", "p", "rest"] {
            assert!(
                bindings.contains(expected),
                "missing {expected}: {bindings:?}"
            );
        }
    }

    #[test]
    fn static_module_specifiers_collects_import_export_require_and_dynamic_import() {
        let source = r#"
            import value from "./a.js";
            export { value } from "./b.js";
            const c = require("./c");
            const d = import("./d.mjs");
            new URL("./asset.png", import.meta.url);
        "#;
        let values = collect_static_module_specifiers(source, None, ParseGoal::TypeScript)
            .expect("parseable")
            .into_iter()
            .map(|fact| fact.value)
            .collect::<Vec<_>>();
        assert_eq!(values, vec!["./a.js", "./b.js", "./c", "./d.mjs"]);
    }
}

#[cfg(test)]
mod import_export_surface_tests {
    use super::collect_module_import_export_surface;
    use crate::errors::ParseGoal;

    fn surface(source: &str) -> super::ModuleImportExportSurface {
        collect_module_import_export_surface(source, None, ParseGoal::TypeScript)
            .expect("parseable")
    }

    #[test]
    fn named_imports_record_the_target_side_wire_name() {
        let s = surface(
            "import { a, b as local } from './m.js';\n\
             import Default from './d.js';\n\
             import * as ns from './n.js';\n\
             import 'side-effect';\n",
        );
        assert_eq!(
            s.named_imports.len(),
            1,
            "only the `{{ … }}` import is named"
        );
        let edge = &s.named_imports[0];
        assert_eq!(edge.specifier, "./m.js");
        // The PUBLIC (target) names — not the importer's local `local`.
        assert_eq!(edge.imported_names, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn exported_surface_uses_public_names_and_flags_star() {
        let s = surface(
            "export const value = 1;\n\
             export function fn() {}\n\
             const local = 2;\n\
             export { local as publicName };\n\
             export { reexported } from './r.js';\n\
             export * as nsExport from './ns.js';\n\
             export default 3;\n",
        );
        for expected in [
            "value",
            "fn",
            "publicName",
            "reexported",
            "nsExport",
            "default",
        ] {
            assert!(
                s.exported_names.contains(expected),
                "missing {expected}: {:?}",
                s.exported_names
            );
        }
        // A bare `export * from` was absent here.
        assert!(!s.has_export_star);
        // The aliased export's LOCAL name is not the public surface.
        assert!(!s.exported_names.contains("local"));
    }

    #[test]
    fn bare_export_star_sets_the_opaque_flag() {
        let s = surface("export * from './all.js';\nexport const named = 1;\n");
        assert!(s.has_export_star);
        assert!(s.exported_names.contains("named"));
    }

    #[test]
    fn type_only_imports_and_exports_are_ignored() {
        let s = surface(
            "import type { T } from './t.js';\n\
             import { value, type U } from './m.js';\n\
             export type { Exported } from './e.js';\n\
             export const runtime = 1;\n",
        );
        // The type-only import binds nothing at runtime; only the value import survives.
        assert_eq!(s.named_imports.len(), 1);
        assert_eq!(s.named_imports[0].imported_names, vec!["value".to_string()]);
        // `Exported` is a type re-export — not part of the runtime surface.
        assert!(!s.exported_names.contains("Exported"));
        assert!(s.exported_names.contains("runtime"));
    }
}
