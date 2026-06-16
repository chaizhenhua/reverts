use std::collections::BTreeMap;
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::{
    Visit,
    ast::{
        ArrowFunctionExpression, BindingPatternKind, Declaration, ExportDefaultDeclarationKind,
        Expression, Function, IdentifierReference, Statement, VariableDeclarator,
    },
    visit::walk::{walk_arrow_function_expression, walk_function},
};
use oxc_parser::Parser;
use oxc_syntax::scope::ScopeFlags;

use crate::errors::ParseGoal;
use crate::facts::collect_identifier_read_facts;
use crate::parse::{parse_options_for, source_type_candidates};

#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclarationCallability {
    Callable,
    NotCallable,
    Unknown,
}

#[must_use]
pub fn classify_top_level_bindings(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> BTreeMap<String, DeclarationCallability> {
    let mut classifications = BTreeMap::new();
    let allocator = Allocator::default();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            continue;
        }
        for statement in &parsed.program.body {
            classify_statement(statement, &mut classifications);
        }
        return classifications;
    }

    classifications
}

fn classify_statement(
    statement: &Statement<'_>,
    classifications: &mut BTreeMap<String, DeclarationCallability>,
) {
    match statement {
        Statement::FunctionDeclaration(function) => {
            if let Some(name) = function.id.as_ref().map(|id| id.name.as_str()) {
                classifications.insert(name.to_string(), DeclarationCallability::Callable);
            }
        }
        Statement::ClassDeclaration(class) => {
            if let Some(name) = class.id.as_ref().map(|id| id.name.as_str()) {
                classifications.insert(name.to_string(), DeclarationCallability::Callable);
            }
        }
        Statement::VariableDeclaration(declaration) => {
            for declarator in &declaration.declarations {
                classify_declarator(declarator, classifications);
            }
        }
        Statement::ExportNamedDeclaration(export) => {
            if let Some(Declaration::FunctionDeclaration(function)) = &export.declaration
                && let Some(name) = function.id.as_ref().map(|id| id.name.as_str())
            {
                classifications.insert(name.to_string(), DeclarationCallability::Callable);
            }
            if let Some(Declaration::ClassDeclaration(class)) = &export.declaration
                && let Some(name) = class.id.as_ref().map(|id| id.name.as_str())
            {
                classifications.insert(name.to_string(), DeclarationCallability::Callable);
            }
            if let Some(Declaration::VariableDeclaration(declaration)) = &export.declaration {
                for declarator in &declaration.declarations {
                    classify_declarator(declarator, classifications);
                }
            }
        }
        Statement::ExportDefaultDeclaration(export) => match &export.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(function) => {
                if let Some(name) = function.id.as_ref().map(|id| id.name.as_str()) {
                    classifications.insert(name.to_string(), DeclarationCallability::Callable);
                }
            }
            ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                if let Some(name) = class.id.as_ref().map(|id| id.name.as_str()) {
                    classifications.insert(name.to_string(), DeclarationCallability::Callable);
                }
            }
            _ => {}
        },
        _ => {}
    }
}

fn classify_declarator(
    declarator: &VariableDeclarator<'_>,
    classifications: &mut BTreeMap<String, DeclarationCallability>,
) {
    let BindingPatternKind::BindingIdentifier(identifier) = &declarator.id.kind else {
        return;
    };
    let name = identifier.name.as_str().to_string();
    let callability = match declarator.init.as_ref() {
        Some(Expression::FunctionExpression(_)) | Some(Expression::ArrowFunctionExpression(_)) => {
            DeclarationCallability::Callable
        }
        Some(Expression::ClassExpression(_)) => DeclarationCallability::Callable,
        Some(
            Expression::NumericLiteral(_)
            | Expression::StringLiteral(_)
            | Expression::BooleanLiteral(_)
            | Expression::NullLiteral(_)
            | Expression::ObjectExpression(_)
            | Expression::ArrayExpression(_)
            | Expression::TemplateLiteral(_),
        ) => DeclarationCallability::NotCallable,
        Some(_) | None => DeclarationCallability::Unknown,
    };
    classifications.insert(name, callability);
}

/// Where in a module's source a binding is observed.
///
/// `TopLevel` — the reference participates in module evaluation order:
/// it appears in a statement that runs when the module is loaded
/// (top-level statement, expression inside a top-level statement,
/// class field initializer, class `static {}` block).
///
/// `NestedOnly` — the reference appears only inside function or arrow
/// bodies. It runs whenever the function is invoked, not when the module
/// is loaded; the import is effectively "lazy" from the consumer's
/// perspective and doesn't constrain the module-eval DAG.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportUsageScope {
    TopLevel,
    NestedOnly,
}

/// Classify every binding in `binding_names` by where it's referenced
/// in `source`. Returns one entry per requested binding (even if the
/// binding is never seen — those map to `NestedOnly` as a vacuously
/// safe default: zero top-level refs means zero constraints). Parse
/// failure is not recovered from: this analysis gates rewrites, so an
/// upstream parser issue must surface instead of turning into a safe
/// classification.
///
/// Use this to decide whether eliminating a lazy thunk that exports a
/// given binding is safe from a module-eval-order perspective: if every
/// consumer's usage is `NestedOnly`, eagerifying the binding cannot
/// reorder evaluation visibly.
#[must_use]
pub fn classify_import_usage_scope(
    source: &str,
    binding_names: &std::collections::BTreeSet<String>,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> BTreeMap<String, ImportUsageScope> {
    let mut out: BTreeMap<String, ImportUsageScope> = binding_names
        .iter()
        .map(|name| (name.clone(), ImportUsageScope::NestedOnly))
        .collect();
    if binding_names.is_empty() {
        return out;
    }
    let allocator = Allocator::default();
    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            continue;
        }
        let mut collector = ImportUsageScopeCollector {
            fn_depth: 0,
            targets: binding_names,
            out: &mut out,
        };
        collector.visit_program(&parsed.program);
        return out;
    }
    panic!("import usage scope classification requires parseable source")
}

struct ImportUsageScopeCollector<'a> {
    /// Number of enclosing function / arrow bodies. Class bodies do
    /// NOT increment this — static-block code and class-field
    /// initializers execute at class declaration time, which is
    /// module-load time when the class itself is declared at the top
    /// level. Methods carry their own `visit_function` frame.
    fn_depth: u32,
    targets: &'a std::collections::BTreeSet<String>,
    out: &'a mut BTreeMap<String, ImportUsageScope>,
}

impl<'a> Visit<'a> for ImportUsageScopeCollector<'_> {
    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        self.fn_depth += 1;
        walk_function(self, it, flags);
        self.fn_depth -= 1;
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        self.fn_depth += 1;
        walk_arrow_function_expression(self, it);
        self.fn_depth -= 1;
    }

    fn visit_identifier_reference(&mut self, identifier: &IdentifierReference<'a>) {
        if self.fn_depth > 0 {
            return;
        }
        let name = identifier.name.as_str();
        if self.targets.contains(name) {
            self.out
                .insert(name.to_string(), ImportUsageScope::TopLevel);
        }
    }
}

/// Whether every reference to a named binding in `source` is a
/// zero-argument call (`X()`) — i.e. the call-site shape that can be
/// mechanically rewritten to a bare reference when the binding is
/// delazified into a direct value.
///
/// Cases that count as "rewritable":
///   * `X()` — counted as `call_count` AND `total_count`.
///
/// Cases that count as `total_count` but NOT `call_count`, making the
/// binding non-rewritable:
///   * `X` as a value (passed to a function, stored, returned, used in
///     `typeof X`, etc.).
///   * `X(args)` — the consumer expects to call the binding with
///     arguments; the binding is being used as a callable value
///     directly, not as a zero-arg thunk.
///   * `X()()`, `X().foo` — chained access on the call result still
///     counts the outer `X()` correctly (call_count++) but only when
///     the inner call has no args.
///
/// Property-key uses (`{ X: 1 }`, `obj.X`) and module-import specifier
/// uses (`import { X } from ...`) are not classified as IdentifierReferences
/// by the AST and so don't appear in either count.
///
/// Returns one entry per requested binding. A binding with zero
/// references is `true` (vacuously safe — no consumer to break).
#[must_use]
pub fn verify_only_immediate_call_references(
    source: &str,
    binding_names: &std::collections::BTreeSet<String>,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> BTreeMap<String, bool> {
    let mut out: BTreeMap<String, bool> = binding_names
        .iter()
        .map(|name| (name.clone(), true))
        .collect();
    if binding_names.is_empty() {
        return out;
    }
    let facts = collect_identifier_read_facts(source, path_hint, goal)
        .expect("call-reference verification requires parseable source");
    let mut call_count = BTreeMap::<String, u32>::new();
    let mut total_count = BTreeMap::<String, u32>::new();
    for fact in facts {
        if !binding_names.contains(fact.name.as_str()) {
            continue;
        }
        *total_count.entry(fact.name.clone()).or_insert(0) += 1;
        if fact.call_arg_count == Some(0) {
            *call_count.entry(fact.name).or_insert(0) += 1;
        }
    }
    for name in binding_names {
        let total = total_count.get(name).copied().unwrap_or(0);
        let calls = call_count.get(name).copied().unwrap_or(0);
        out.insert(name.clone(), total == 0 || total == calls);
    }
    out
}
