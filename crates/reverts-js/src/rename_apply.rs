use std::collections::{BTreeMap, BTreeSet};

use oxc_allocator::Allocator;
use oxc_ast::{
    AstBuilder, Visit, VisitMut,
    ast::{
        BindingIdentifier, BindingPatternKind, Class, ClassElement, ExportNamedDeclaration,
        Expression, FormalParameters, Function, IdentifierReference, ImportDeclaration,
        ImportDeclarationSpecifier, MethodDefinitionKind, PropertyKey, Program, VariableDeclarator,
    },
};
use oxc_semantic::SemanticBuilder;
use oxc_span::SPAN;
use oxc_syntax::{reference::ReferenceId, scope::ScopeFlags, symbol::SymbolId};

use crate::identifier::sanitize_identifier;
use crate::module_export_name_text;
use crate::{GeneratedRename, ReadabilityReport};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ReadabilityRenameSource {
    UsagePattern,
    ObjectProperty,
    PackageNamespace,
    ImportExportPublic,
    CommonJsExport,
    ExplicitSemantic,
}

impl ReadabilityRenameSource {
    fn confidence(self) -> u8 {
        match self {
            Self::ExplicitSemantic => 100,
            Self::ImportExportPublic | Self::CommonJsExport => 90,
            Self::PackageNamespace => 80,
            Self::ObjectProperty => 50,
            Self::UsagePattern => 40,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitSemantic => "explicit_semantic",
            Self::ImportExportPublic => "import_export_public",
            Self::CommonJsExport => "commonjs_export",
            Self::PackageNamespace => "package_namespace",
            Self::ObjectProperty => "object_property",
            Self::UsagePattern => "usage_pattern",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ReadabilityRenameHint {
    original: String,
    renamed: String,
    source: ReadabilityRenameSource,
}

impl ReadabilityRenameHint {
    pub(crate) fn new(original: &str, renamed: &str, source: ReadabilityRenameSource) -> Self {
        Self {
            original: original.trim().to_string(),
            renamed: renamed.trim().to_string(),
            source,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedReadabilityRename {
    original: String,
    renamed: String,
    source: ReadabilityRenameSource,
}

pub(crate) fn resolve_readability_rename_hints(
    hints: Vec<ReadabilityRenameHint>,
    report: &mut ReadabilityReport,
) -> Vec<ResolvedReadabilityRename> {
    let mut hints_by_original = BTreeMap::<String, Vec<ReadabilityRenameHint>>::new();
    for hint in hints {
        if hint.original.is_empty() || hint.renamed.is_empty() || hint.original == hint.renamed {
            continue;
        }
        if sanitize_identifier(hint.renamed.as_str()) != hint.renamed {
            report.push(format!(
                "skipped rename {} -> {}, source={}, reason=invalid_target",
                hint.original,
                hint.renamed,
                hint.source.as_str()
            ));
            continue;
        }
        hints_by_original
            .entry(hint.original.clone())
            .or_default()
            .push(hint);
    }

    let mut resolved = Vec::new();
    for (original, hints) in hints_by_original {
        let max_confidence = hints
            .iter()
            .map(|hint| hint.source.confidence())
            .max()
            .unwrap_or(0);
        let top_hints = hints
            .iter()
            .filter(|hint| hint.source.confidence() == max_confidence)
            .collect::<Vec<_>>();
        let top_names = top_hints
            .iter()
            .map(|hint| hint.renamed.as_str())
            .collect::<BTreeSet<_>>();
        if top_names.len() != 1 {
            let candidates = top_hints
                .iter()
                .map(|hint| format!("{}:{}", hint.source.as_str(), hint.renamed))
                .collect::<Vec<_>>()
                .join(", ");
            report.push(format!(
                "skipped rename {}, reason=conflicting_hints, candidates={candidates}",
                original
            ));
            continue;
        }
        let chosen = top_hints[0];
        resolved.push(ResolvedReadabilityRename {
            original,
            renamed: chosen.renamed.clone(),
            source: chosen.source,
        });
    }
    resolved
}

pub(crate) fn apply_readability_renames<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    readability_renames: &[ResolvedReadabilityRename],
    report: &mut ReadabilityReport,
) {
    let requested = readability_renames
        .iter()
        .map(|rename| (rename.original.clone(), rename.clone()))
        .collect::<BTreeMap<_, _>>();
    if requested.is_empty() {
        return;
    }

    let (symbol_renames, reference_renames) = {
        let semantic = SemanticBuilder::new().build(program).semantic;
        let symbols = semantic.symbols();
        let root_scope_id = semantic.scopes().root_scope_id();
        let unresolved_root_names = semantic
            .scopes()
            .root_unresolved_references()
            .keys()
            .map(|name| name.as_str().to_string())
            .collect::<BTreeSet<_>>();
        let root_symbols = symbols
            .symbol_ids()
            .filter(|symbol_id| symbols.get_scope_id(*symbol_id) == root_scope_id)
            .collect::<Vec<_>>();

        let mut symbol_renames = BTreeMap::<SymbolId, String>::new();
        for (original, rename) in &requested {
            let renamed = rename.renamed.as_str();
            // If the desired name is already used as a free global reference,
            // introducing a module-scope binding with that name would change
            // resolution for nested reads. Leave the original name intact.
            if unresolved_root_names.contains(renamed) {
                report.push(format!(
                    "skipped rename {original} -> {renamed}, source={}, reason=would_capture_global",
                    rename.source.as_str()
                ));
                continue;
            }
            let targets = root_symbols
                .iter()
                .copied()
                .filter(|symbol_id| symbols.get_name(*symbol_id) == original)
                .collect::<Vec<_>>();
            if targets.len() != 1 {
                report.push(format!(
                    "skipped rename {original} -> {renamed}, source={}, reason=missing_or_ambiguous_original",
                    rename.source.as_str()
                ));
                continue;
            }
            let target = targets[0];
            let collides = root_symbols
                .iter()
                .copied()
                .any(|symbol_id| symbol_id != target && symbols.get_name(symbol_id) == renamed);
            if collides {
                report.push(format!(
                    "skipped rename {original} -> {renamed}, source={}, reason=name_collision",
                    rename.source.as_str()
                ));
                continue;
            }
            if symbol_renames.values().any(|value| value == renamed) {
                report.push(format!(
                    "skipped rename {original} -> {renamed}, source={}, reason=duplicate_target",
                    rename.source.as_str()
                ));
                continue;
            }
            symbol_renames.insert(target, renamed.to_string());
            report.push(format!(
                "renamed {original} -> {renamed}, source={}",
                rename.source.as_str()
            ));
        }

        let mut reference_renames = BTreeMap::<ReferenceId, String>::new();
        for (symbol_id, renamed) in &symbol_renames {
            for reference_id in symbols.get_resolved_reference_ids(*symbol_id) {
                reference_renames.insert(*reference_id, renamed.clone());
            }
        }

        (symbol_renames, reference_renames)
    };

    if symbol_renames.is_empty() && reference_renames.is_empty() {
        return;
    }

    let mut renamer = ReadabilityRenamer {
        builder: AstBuilder::new(allocator),
        symbol_renames,
        reference_renames,
    };
    renamer.visit_program(program);
}

pub(crate) fn apply_all_scope_readability_renames<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    readability_renames: &[GeneratedRename],
    report: &mut ReadabilityReport,
) {
    let requested_all = readability_renames
        .iter()
        .filter(|rename| rename.scope == crate::GeneratedRenameScope::All)
        .map(|rename| (rename.original.clone(), rename.renamed.clone()))
        .collect::<BTreeMap<_, _>>();
    let requested_by_binding = readability_renames
        .iter()
        .filter_map(|rename| match rename.scope {
            crate::GeneratedRenameScope::BindingIndex(binding_index) => Some((
                (rename.original.clone(), binding_index),
                rename.renamed.clone(),
            )),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    if requested_all.is_empty() && requested_by_binding.is_empty() {
        return;
    }

    let (symbol_renames, reference_renames) = {
        let semantic = SemanticBuilder::new().build(program).semantic;
        let symbols = semantic.symbols();
        let symbol_ids = symbols.symbol_ids().collect::<Vec<_>>();
        let mut binding_ordinals = BindingOrdinalCollector::default();
        binding_ordinals.visit_program(program);
        let mut catch_params = CatchParamCollector::default();
        catch_params.visit_program(program);
        let mut symbol_names_by_scope = BTreeMap::<_, BTreeMap<String, Vec<SymbolId>>>::new();
        for symbol_id in &symbol_ids {
            symbol_names_by_scope
                .entry(symbols.get_scope_id(*symbol_id))
                .or_default()
                .entry(symbols.get_name(*symbol_id).to_string())
                .or_default()
                .push(*symbol_id);
        }

        let mut requested_targets_by_scope = BTreeMap::<_, BTreeSet<String>>::new();
        let mut symbol_renames = BTreeMap::<SymbolId, String>::new();
        for symbol_id in symbol_ids {
            let original = symbols.get_name(symbol_id);
            let binding_index = binding_ordinals
                .symbol_binding_indices
                .get(&symbol_id)
                .copied();
            let indexed_key = binding_index.map(|index| (original.to_string(), index));
            let renamed = if let Some(renamed) = indexed_key
                .as_ref()
                .and_then(|key| requested_by_binding.get(key))
            {
                // Explicit per-binding rename (precise ordinal) — honored as-is.
                renamed
            } else if let Some(renamed) = requested_all.get(original) {
                // All-scope rename: renames EVERY same-named binding. A reused
                // minified name must not capture a catch-clause parameter (an error
                // binding), so skip those — the fix for `catch (processElements)`.
                if catch_params.symbols.contains(&symbol_id) {
                    report.push(format!(
                        "skipped rename {original} -> {renamed}, source=explicit_binding_semantic, reason=catch_parameter"
                    ));
                    continue;
                }
                renamed
            } else {
                continue;
            };
            let scope_id = symbols.get_scope_id(symbol_id);
            let same_scope_names = symbol_names_by_scope.entry(scope_id).or_default();
            let collides = same_scope_names
                .get(renamed)
                .is_some_and(|ids| ids.iter().any(|id| *id != symbol_id));
            if collides {
                report.push(format!(
                    "skipped rename {original} -> {renamed}, source=explicit_binding_semantic, reason=name_collision"
                ));
                continue;
            }
            if !requested_targets_by_scope
                .entry(scope_id)
                .or_default()
                .insert(renamed.clone())
            {
                report.push(format!(
                    "skipped rename {original} -> {renamed}, source=explicit_binding_semantic, reason=duplicate_target"
                ));
                continue;
            }
            symbol_renames.insert(symbol_id, renamed.clone());
            report.push(format!(
                "renamed {original} -> {renamed}, source=explicit_binding_semantic"
            ));
        }

        let mut reference_renames = BTreeMap::<ReferenceId, String>::new();
        for (symbol_id, renamed) in &symbol_renames {
            for reference_id in symbols.get_resolved_reference_ids(*symbol_id) {
                reference_renames.insert(*reference_id, renamed.clone());
            }
        }

        (symbol_renames, reference_renames)
    };

    if symbol_renames.is_empty() && reference_renames.is_empty() {
        return;
    }

    let mut renamer = ReadabilityRenamer {
        builder: AstBuilder::new(allocator),
        symbol_renames,
        reference_renames,
    };
    renamer.visit_program(program);
}

/// Collects the symbol ids bound by `catch (param)` clauses. An `All`-scope
/// readability rename renames EVERY same-named binding in the file, which wrongly
/// captures a reused minified catch-parameter (producing nonsense like
/// `catch (processElements)` / `catch (onAbort)` — an error binding named after an
/// unrelated recovered function). Catch parameters are never exported and their
/// names are not meaningfully recoverable, so they are excluded from All-scope
/// renames.
#[derive(Default)]
struct CatchParamCollector {
    symbols: BTreeSet<SymbolId>,
    depth: u32,
}

impl<'a> Visit<'a> for CatchParamCollector {
    fn visit_program(&mut self, program: &Program<'a>) {
        oxc_ast::visit::walk::walk_program(self, program);
    }

    fn visit_catch_parameter(&mut self, it: &oxc_ast::ast::CatchParameter<'a>) {
        self.depth += 1;
        oxc_ast::visit::walk::walk_catch_parameter(self, it);
        self.depth -= 1;
    }

    fn visit_binding_identifier(&mut self, identifier: &BindingIdentifier<'a>) {
        if self.depth > 0
            && let Some(symbol_id) = identifier.symbol_id.get()
        {
            self.symbols.insert(symbol_id);
        }
    }
}

#[derive(Default)]
struct BindingOrdinalCollector {
    name_counts: BTreeMap<String, u32>,
    symbol_binding_indices: BTreeMap<SymbolId, u32>,
}

impl<'a> Visit<'a> for BindingOrdinalCollector {
    fn visit_program(&mut self, program: &Program<'a>) {
        oxc_ast::visit::walk::walk_program(self, program);
    }

    fn visit_binding_identifier(&mut self, identifier: &BindingIdentifier<'a>) {
        let Some(symbol_id) = identifier.symbol_id.get() else {
            return;
        };
        let index = self
            .name_counts
            .entry(identifier.name.as_str().to_string())
            .and_modify(|count| *count = count.saturating_add(1))
            .or_insert(1);
        self.symbol_binding_indices.insert(symbol_id, *index);
    }
}

pub(crate) fn apply_emit_safety_renames<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    report: &mut ReadabilityReport,
) {
    let (symbol_renames, reference_renames) = {
        let semantic = SemanticBuilder::new().build(program).semantic;
        let symbols = semantic.symbols();
        let mut used_names = symbols
            .symbol_ids()
            .map(|symbol_id| symbols.get_name(symbol_id).to_string())
            .collect::<BTreeSet<_>>();
        used_names.extend(
            semantic
                .scopes()
                .root_unresolved_references()
                .keys()
                .map(|name| name.as_str().to_string()),
        );

        let mut symbol_renames = BTreeMap::<SymbolId, String>::new();
        let mut reference_renames = BTreeMap::<ReferenceId, String>::new();
        for symbol_id in symbols.symbol_ids() {
            let original = symbols.get_name(symbol_id);
            let sanitized = sanitize_identifier(original);
            if sanitized == original {
                continue;
            }
            let renamed = unique_safe_identifier(&sanitized, &mut used_names);
            symbol_renames.insert(symbol_id, renamed.clone());
            for reference_id in symbols.get_resolved_reference_ids(symbol_id) {
                reference_renames.insert(*reference_id, renamed.clone());
            }
            report.push(format!(
                "renamed {original} -> {renamed}, source=emit_safety"
            ));
        }

        (symbol_renames, reference_renames)
    };

    if symbol_renames.is_empty() && reference_renames.is_empty() {
        return;
    }

    let mut renamer = ReadabilityRenamer {
        builder: AstBuilder::new(allocator),
        symbol_renames,
        reference_renames,
    };
    renamer.visit_program(program);
}

/// A request to rename the `param_index`-th formal parameter of the (uniquely
/// named) function `function` to `renamed`. Produced by cross-version matching:
/// a matched function's minified positional parameters take the matched
/// reference's real parameter names. Keyed by function name + position — NOT a
/// file-global binding ordinal — so it is immune to the import-insertion and
/// earlier-rename ordinal drift that makes `BindingIndex` unreliable for params.
#[derive(Debug, Clone)]
pub struct FunctionParamRename {
    pub function: String,
    pub param_index: u32,
    pub renamed: String,
}

/// Rename matched functions' minified positional parameters to real names.
///
/// Soundness: a parameter is renamed only when (a) exactly one function in the
/// file resolves to the requested name (no ambiguity), (b) the parameter at that
/// position is a plain identifier binding with a resolved symbol, and (c) the new
/// name does not already occur anywhere inside that function — so the rename can
/// never merge two distinct bindings or capture a free reference. Functions whose
/// name resolves through forms this pass does not model (object/class methods,
/// member assignments) are simply left untouched. The rename targets the
/// parameter's `SymbolId` and every resolved reference, so all in-scope uses move
/// together.
pub(crate) fn apply_function_param_renames<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    renames: &[FunctionParamRename],
    report: &mut ReadabilityReport,
) {
    if renames.is_empty() {
        return;
    }
    let mut wanted: BTreeMap<&str, BTreeMap<u32, &str>> = BTreeMap::new();
    for rename in renames {
        wanted
            .entry(rename.function.as_str())
            .or_default()
            .insert(rename.param_index, rename.renamed.as_str());
    }

    let (symbol_renames, reference_renames) = {
        let semantic = SemanticBuilder::new().build(program).semantic;
        let symbols = semantic.symbols();

        let mut locator = FunctionParamLocator {
            wanted: &wanted,
            functions: BTreeMap::new(),
            ambiguous: BTreeSet::new(),
        };
        locator.visit_program(program);

        let mut symbol_renames = BTreeMap::<SymbolId, String>::new();
        let mut reference_renames = BTreeMap::<ReferenceId, String>::new();
        for (&function_name, indices) in &wanted {
            if locator.ambiguous.contains(function_name) {
                continue;
            }
            let Some(located) = locator.functions.get(function_name) else {
                continue;
            };
            for (&index, &new_name) in indices {
                let Some(Some(symbol_id)) = located.param_symbols.get(index as usize).copied()
                else {
                    continue;
                };
                let original = symbols.get_name(symbol_id);
                if original == new_name {
                    continue;
                }
                // Collision guard: the new name must not already appear anywhere
                // inside this function, else the rename would merge or capture it.
                if located.used_names.contains(new_name) {
                    report.push(format!(
                        "skipped param rename {original} -> {new_name} in {function_name}, source=function_param, reason=name_in_use"
                    ));
                    continue;
                }
                symbol_renames.insert(symbol_id, new_name.to_string());
                for reference_id in symbols.get_resolved_reference_ids(symbol_id) {
                    reference_renames.insert(*reference_id, new_name.to_string());
                }
                report.push(format!(
                    "renamed param {original} -> {new_name} in {function_name}, source=function_param"
                ));
            }
        }
        (symbol_renames, reference_renames)
    };

    if symbol_renames.is_empty() && reference_renames.is_empty() {
        return;
    }
    let mut renamer = ReadabilityRenamer {
        builder: AstBuilder::new(allocator),
        symbol_renames,
        reference_renames,
    };
    renamer.visit_program(program);
}

struct LocatedFunction {
    /// Symbol id of each formal parameter in source order (`None` for a
    /// destructured/rest param, or one without a resolved symbol).
    param_symbols: Vec<Option<SymbolId>>,
    /// Every identifier name appearing anywhere inside the function — used to
    /// reject renames that would collide with an existing binding or reference.
    used_names: BTreeSet<String>,
}

/// Resolves function names the way the matcher's `function_names` does for the
/// dominant first-party forms — `function NAME(params){}`,
/// `const NAME = function/arrow(params)`, and class methods addressed by the
/// class-qualified key `Class.method` (incl. `Class.constructor`) — recording
/// each requested function's parameter symbols. A name seen more than once is
/// marked ambiguous and skipped.
struct FunctionParamLocator<'r> {
    wanted: &'r BTreeMap<&'r str, BTreeMap<u32, &'r str>>,
    functions: BTreeMap<String, LocatedFunction>,
    ambiguous: BTreeSet<String>,
}

impl<'r> FunctionParamLocator<'r> {
    fn record(&mut self, name: &str, params: &FormalParameters<'_>, used_names: BTreeSet<String>) {
        if !self.wanted.contains_key(name) {
            return;
        }
        if self.functions.contains_key(name) {
            self.ambiguous.insert(name.to_string());
            return;
        }
        let param_symbols = params
            .items
            .iter()
            .map(|param| simple_param_symbol(&param.pattern.kind))
            .collect();
        self.functions.insert(
            name.to_string(),
            LocatedFunction {
                param_symbols,
                used_names,
            },
        );
    }
}

impl<'a, 'r> Visit<'a> for FunctionParamLocator<'r> {
    fn visit_function(&mut self, function: &Function<'a>, flags: ScopeFlags) {
        if let Some(id) = &function.id {
            let used = collect_used_identifier_names(|collector| {
                collector.visit_function(function, flags);
            });
            self.record(id.name.as_str(), &function.params, used);
        }
        oxc_ast::visit::walk::walk_function(self, function, flags);
    }

    fn visit_variable_declarator(&mut self, declarator: &VariableDeclarator<'a>) {
        if let BindingPatternKind::BindingIdentifier(id) = &declarator.id.kind
            && let Some(init) = &declarator.init
            && let Some((params, used)) = function_like_params_and_uses(init)
        {
            self.record(id.name.as_str(), params, used);
        }
        oxc_ast::visit::walk::walk_variable_declarator(self, declarator);
    }

    fn visit_class(&mut self, class: &Class<'a>) {
        // Class methods are addressed by the class-qualified key `Class.method`
        // (and `Class.constructor`), so two classes in one file with same-named
        // methods don't collide into "ambiguous". Anonymous classes and computed
        // method keys have no stable key and are skipped. `record` itself fast-
        // rejects keys not in `wanted`, but we pre-check before collecting used
        // names to avoid that walk on every method.
        if let Some(class_name) = class.id.as_ref().map(|id| id.name.as_str()) {
            for element in &class.body.body {
                let ClassElement::MethodDefinition(method) = element else {
                    continue;
                };
                if method.computed {
                    continue;
                }
                let Some(method_name) = property_key_name(&method.key) else {
                    continue;
                };
                // Getters/setters share their accessor name with each other and
                // with a same-named field; their parameter shape (0/1) is not a
                // useful naming target, so skip them.
                if matches!(
                    method.kind,
                    MethodDefinitionKind::Get | MethodDefinitionKind::Set
                ) {
                    continue;
                }
                let qualified = format!("{class_name}.{method_name}");
                if !self.wanted.contains_key(qualified.as_str()) {
                    continue;
                }
                let used = collect_used_identifier_names(|collector| {
                    collector.visit_function(&method.value, ScopeFlags::empty());
                });
                self.record(qualified.as_str(), &method.value.params, used);
            }
        }
        oxc_ast::visit::walk::walk_class(self, class);
    }
}

/// The static text of a non-computed property key (`identifier`, `"string"`),
/// or `None` for computed / private keys.
fn property_key_name(key: &PropertyKey<'_>) -> Option<String> {
    match key {
        PropertyKey::StaticIdentifier(identifier) => Some(identifier.name.to_string()),
        PropertyKey::StringLiteral(literal) => Some(literal.value.to_string()),
        _ => None,
    }
}

/// Parameters and in-function identifier names of a `function`/arrow expression
/// (peeling parentheses), or `None` if the expression is not function-like.
fn function_like_params_and_uses<'a>(
    expr: &'a Expression<'a>,
) -> Option<(&'a FormalParameters<'a>, BTreeSet<String>)> {
    match expr {
        Expression::FunctionExpression(function) => {
            let used = collect_used_identifier_names(|collector| {
                collector.visit_function(function, ScopeFlags::empty());
            });
            Some((&function.params, used))
        }
        Expression::ArrowFunctionExpression(arrow) => {
            let used = collect_used_identifier_names(|collector| {
                collector.visit_arrow_function_expression(arrow);
            });
            Some((&arrow.params, used))
        }
        Expression::ParenthesizedExpression(paren) => {
            function_like_params_and_uses(&paren.expression)
        }
        _ => None,
    }
}

fn simple_param_symbol(kind: &BindingPatternKind<'_>) -> Option<SymbolId> {
    match kind {
        BindingPatternKind::BindingIdentifier(ident) => ident.symbol_id.get(),
        BindingPatternKind::AssignmentPattern(assignment) => {
            simple_param_symbol(&assignment.left.kind)
        }
        BindingPatternKind::ObjectPattern(_) | BindingPatternKind::ArrayPattern(_) => None,
    }
}

fn collect_used_identifier_names(
    walk: impl FnOnce(&mut IdentifierNameCollector),
) -> BTreeSet<String> {
    let mut collector = IdentifierNameCollector {
        names: BTreeSet::new(),
    };
    walk(&mut collector);
    collector.names
}

struct IdentifierNameCollector {
    names: BTreeSet<String>,
}

impl<'a> Visit<'a> for IdentifierNameCollector {
    fn visit_binding_identifier(&mut self, identifier: &BindingIdentifier<'a>) {
        self.names.insert(identifier.name.to_string());
    }
    fn visit_identifier_reference(&mut self, identifier: &IdentifierReference<'a>) {
        self.names.insert(identifier.name.to_string());
    }
}

fn unique_safe_identifier(base: &str, used_names: &mut BTreeSet<String>) -> String {
    if !used_names.contains(base) {
        used_names.insert(base.to_string());
        return base.to_string();
    }
    for suffix in 2.. {
        let candidate = format!("{base}{suffix}");
        if !used_names.contains(&candidate) {
            used_names.insert(candidate.clone());
            return candidate;
        }
    }
    unreachable!("unbounded suffix search should always find an identifier")
}

/// After local readability renames have run, an exported/imported binding reads
/// semantically at its declaration and use sites but the module *wire* name is
/// still the minified original — the emitter left an alias
/// (`import { Cb as parseDocument }`, `export { parseDocument as Cb }`). For
/// bindings the planner proved safe to rename project-wide (`wire_renames`
/// carries `(minified_wire_name, semantic_name)` pairs), this pass rewrites the
/// wire name to match the local, collapsing the alias to
/// `import { parseDocument }` / `export { parseDocument }`.
///
/// It rewrites ONLY a specifier whose local was actually renamed to the semantic
/// name (`imported == original && local == renamed`), so a same-named specifier
/// the rename did not touch is left alone. Re-export barrels
/// (`export { x } from './m'`) are skipped; the planner's gate excludes any
/// binding that is re-exported under a different name, consumed via a namespace
/// import, or whose semantic name is not unique project-wide.
pub(crate) fn apply_wire_export_import_renames<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    wire_renames: &[GeneratedRename],
    importer_path: Option<&std::path::Path>,
) {
    let by_wire: BTreeMap<&str, &str> = wire_renames
        .iter()
        .map(|rename| (rename.original.as_str(), rename.renamed.as_str()))
        .collect();
    if by_wire.is_empty() {
        return;
    }
    // Import-side renames that carry the defining module's path: these rewrite an
    // *aliased* import (`import { o as v }`, local != renamed) by matching the
    // import's `from` source to that module. The dir the importer lives in is
    // needed to resolve relative sources.
    let importer_dir = importer_path.and_then(|p| p.parent().map(std::path::Path::to_path_buf));
    let module_wire: Vec<(&str, &str, String)> = wire_renames
        .iter()
        .filter_map(|rename| {
            rename.wire_source.as_deref().map(|source| {
                (
                    rename.original.as_str(),
                    rename.renamed.as_str(),
                    strip_source_extension(source),
                )
            })
        })
        .collect();
    let mut renamer = WireRenamer {
        builder: AstBuilder::new(allocator),
        by_wire,
        module_wire,
        importer_dir,
    };
    renamer.visit_program(program);
}

/// Lexically resolve `specifier` (a relative import, e.g. `../a/b.js`) against
/// `base_dir` and strip the extension, so it can be compared to a defining
/// module's recorded path. No filesystem access; `.`/`..` segments are folded.
fn resolved_import_module(base_dir: Option<&std::path::Path>, specifier: &str) -> Option<String> {
    if !specifier.starts_with('.') {
        return None; // bare/package import — not a first-party module path
    }
    let base = base_dir?;
    let joined = base.join(specifier);
    let mut parts: Vec<&str> = Vec::new();
    for comp in joined.to_str()?.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    Some(strip_source_extension(&parts.join("/")))
}

fn strip_source_extension(path: &str) -> String {
    for ext in [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".d.ts"] {
        if let Some(stripped) = path.strip_suffix(ext) {
            return stripped.to_string();
        }
    }
    path.to_string()
}

struct WireRenamer<'a, 'm> {
    builder: AstBuilder<'a>,
    by_wire: BTreeMap<&'m str, &'m str>,
    module_wire: Vec<(&'m str, &'m str, String)>,
    importer_dir: Option<std::path::PathBuf>,
}

impl<'a> VisitMut<'a> for WireRenamer<'a, '_> {
    fn visit_import_declaration(&mut self, declaration: &mut ImportDeclaration<'a>) {
        let source = declaration.source.value.as_str().to_string();
        let Some(specifiers) = declaration.specifiers.as_mut() else {
            return;
        };
        // The module this import reads from, resolved against the importer's dir.
        let from_module = resolved_import_module(self.importer_dir.as_deref(), source.as_str());
        for specifier in specifiers.iter_mut() {
            let ImportDeclarationSpecifier::ImportSpecifier(import) = specifier else {
                continue;
            };
            let Some(imported) = module_export_name_text(&import.imported) else {
                continue;
            };
            // Shorthand collapse: the local was readability-renamed to the
            // semantic name, so `import { o as s }` → `import { s }`. Safe by the
            // `local == s` signal alone (no module match needed).
            if let Some(renamed) = self.by_wire.get(imported.as_str())
                && import.local.name.as_str() == *renamed
            {
                import.imported = self
                    .builder
                    .module_export_name_identifier_name(SPAN, *renamed);
                continue;
            }
            // Aliased import (`import { o as v }`, local is the importer's own
            // name): rewrite the imported name only when the import's source
            // module is the one whose export was collapsed to the semantic name.
            if let Some(from) = from_module.as_deref()
                && let Some((_, renamed, _)) = self
                    .module_wire
                    .iter()
                    .find(|(o, _, src)| *o == imported.as_str() && src == from)
            {
                import.imported = self
                    .builder
                    .module_export_name_identifier_name(SPAN, *renamed);
            }
        }
    }

    fn visit_export_named_declaration(&mut self, declaration: &mut ExportNamedDeclaration<'a>) {
        // Only the defining module's own `export { x }`; a re-export barrel
        // (`export { x } from './m'`) carries a source and is left untouched.
        if declaration.source.is_some() {
            return;
        }
        for specifier in declaration.specifiers.iter_mut() {
            let (Some(local), Some(exported)) = (
                module_export_name_text(&specifier.local),
                module_export_name_text(&specifier.exported),
            ) else {
                continue;
            };
            if let Some(renamed) = self.by_wire.get(exported.as_str())
                && local.as_str() == *renamed
            {
                specifier.exported = self
                    .builder
                    .module_export_name_identifier_name(SPAN, *renamed);
            }
        }
    }
}

struct ReadabilityRenamer<'a> {
    builder: AstBuilder<'a>,
    symbol_renames: BTreeMap<SymbolId, String>,
    reference_renames: BTreeMap<ReferenceId, String>,
}

impl<'a> VisitMut<'a> for ReadabilityRenamer<'a> {
    fn visit_binding_identifier(&mut self, identifier: &mut BindingIdentifier<'a>) {
        let Some(symbol_id) = identifier.symbol_id.get() else {
            return;
        };
        let Some(renamed) = self.symbol_renames.get(&symbol_id) else {
            return;
        };
        identifier.name = self.builder.atom(renamed);
    }

    fn visit_identifier_reference(&mut self, identifier: &mut IdentifierReference<'a>) {
        let Some(reference_id) = identifier.reference_id.get() else {
            return;
        };
        let Some(renamed) = self.reference_renames.get(&reference_id) else {
            return;
        };
        identifier.name = self.builder.atom(renamed);
    }
}

#[cfg(test)]
mod function_param_rename_tests {
    use super::*;
    use oxc_codegen::CodeGenerator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn run(source: &str, renames: &[FunctionParamRename]) -> String {
        let allocator = Allocator::default();
        let source_type = SourceType::default().with_typescript(true);
        let mut parsed = Parser::new(&allocator, source, source_type).parse();
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        let mut report = ReadabilityReport::default();
        apply_function_param_renames(&allocator, &mut parsed.program, renames, &mut report);
        CodeGenerator::new().build(&parsed.program).code
    }

    fn rename(function: &str, index: u32, renamed: &str) -> FunctionParamRename {
        FunctionParamRename {
            function: function.to_string(),
            param_index: index,
            renamed: renamed.to_string(),
        }
    }

    #[test]
    fn renames_positional_params_and_their_references() {
        let out = run(
            "function rFz(q, K) { return q + K; }",
            &[rename("rFz", 0, "value"), rename("rFz", 1, "count")],
        );
        assert!(out.contains("function rFz(value, count)"), "got: {out}");
        assert!(out.contains("return value + count"), "got: {out}");
        assert!(!out.contains('q') && !out.contains('K'), "got: {out}");
    }

    #[test]
    fn renames_arrow_assigned_to_a_const_binding() {
        let out = run(
            "const handler = (a, b) => a.use(b);",
            &[
                rename("handler", 0, "node"),
                rename("handler", 1, "options"),
            ],
        );
        assert!(out.contains("(node, options) =>"), "got: {out}");
        assert!(out.contains("node.use(options)"), "got: {out}");
    }

    #[test]
    fn skips_ambiguous_function_name() {
        // Two functions named `g` — renaming either is unsafe, so neither moves.
        let out = run(
            "function g(q) { return q; } function g(K) { return K; }",
            &[rename("g", 0, "value")],
        );
        assert!(out.contains("function g(q)"), "got: {out}");
        assert!(out.contains("function g(K)"), "got: {out}");
        assert!(!out.contains("value"), "got: {out}");
    }

    #[test]
    fn renames_class_method_params_via_qualified_key() {
        let out = run(
            "class BatchQueue { enqueue(e) { this.pending.push(e); } }",
            &[rename("BatchQueue.enqueue", 0, "items")],
        );
        assert!(out.contains("enqueue(items)"), "got: {out}");
        assert!(out.contains("this.pending.push(items)"), "got: {out}");
    }

    #[test]
    fn renames_constructor_param_via_qualified_key() {
        let out = run(
            "class BatchQueue { constructor(e) { this.config = e; } }",
            &[rename("BatchQueue.constructor", 0, "config")],
        );
        assert!(out.contains("constructor(config)"), "got: {out}");
        assert!(out.contains("this.config = config"), "got: {out}");
    }

    #[test]
    fn same_method_name_in_two_classes_does_not_collide() {
        // Both classes have `run`; the class-qualified key keeps them distinct, so
        // each renames independently rather than being marked ambiguous.
        let out = run(
            "class A { run(e) { return e; } } class B { run(e) { return e + 1; } }",
            &[
                rename("A.run", 0, "value"),
                rename("B.run", 0, "count"),
            ],
        );
        assert!(out.contains("run(value)"), "A not renamed: {out}");
        assert!(out.contains("run(count)"), "B not renamed: {out}");
    }

    #[test]
    fn skips_bare_method_name_without_class_qualifier() {
        // A bare `enqueue` key must not match a class method — only the qualified
        // `Class.enqueue` form addresses methods.
        let out = run(
            "class BatchQueue { enqueue(e) { return e; } }",
            &[rename("enqueue", 0, "items")],
        );
        assert!(out.contains("enqueue(e)"), "bare key wrongly matched: {out}");
        assert!(!out.contains("items"), "got: {out}");
    }

    #[test]
    fn skips_rename_when_new_name_already_used_in_function() {
        // `value` already exists as a local — renaming `q` to it would merge them.
        let out = run(
            "function h(q) { let value = 1; return q + value; }",
            &[rename("h", 0, "value")],
        );
        assert!(
            out.contains("function h(q)"),
            "collision not skipped: {out}"
        );
        assert!(out.contains("let value = 1"), "got: {out}");
    }

    #[test]
    fn renames_only_the_targeted_function_scopes_param_not_a_nested_shadow() {
        // `outer` and `inner` each bind `q`; they are distinct symbols. Renaming
        // outer's param 0 must move only outer's `q`, leaving inner's intact.
        let out = run(
            "function outer(q) { function inner(q) { return q; } return q + inner(1); }",
            &[rename("outer", 0, "root")],
        );
        assert!(out.contains("function outer(root)"), "got: {out}");
        assert!(
            out.contains("function inner(q)"),
            "nested shadow moved: {out}"
        );
        assert!(out.contains("return root + inner(1)"), "got: {out}");
        assert!(out.contains("return q"), "inner body shadow moved: {out}");
    }

    #[test]
    fn leaves_destructured_param_slot_untouched_but_renames_a_sibling() {
        let out = run(
            "function d({ mode: m }, q) { return m + q; }",
            &[rename("d", 0, "config"), rename("d", 1, "value")],
        );
        // index 0 is destructured -> no transferable symbol -> unchanged key.
        assert!(
            out.contains("{ mode: m }"),
            "destructured slot changed: {out}"
        );
        assert!(out.contains(", value)"), "sibling not renamed: {out}");
        assert!(out.contains("return m + value"), "got: {out}");
    }
}
