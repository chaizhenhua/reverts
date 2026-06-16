use std::collections::BTreeMap;
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::{
    Visit,
    ast::{
        Argument, ArrowFunctionExpression, BindingPatternKind, CallExpression,
        ExportNamedDeclaration, Expression, Function, IdentifierReference, Program, Statement,
    },
    visit::walk::{walk_call_expression, walk_export_named_declaration, walk_expression},
};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType, Span};

use crate::errors::ParseGoal;
use crate::module_export_name_text;
use crate::parse::{parse_options_for, source_type_candidates};

/// Compiler-specific lowering applied when re-emitting a parsed module body.
/// Each variant names a recognised bundler/transpiler whose semantically
/// no-op artefacts can be stripped from the emitted output.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CompilerLowering {
    #[default]
    None,
    Babel,
    Esbuild,
    Webpack,
}

/// Canonical webpack runtime helper names. Webpack 5 emits these at the top
/// of its IIFE bootstrap; when `output.iife: false` the same definitions
/// appear at module scope where this strip applies directly. The IIFE-
/// wrapped case is left for a future slice that descends into the IIFE.
pub(crate) const WEBPACK_RUNTIME_HELPERS: &[&str] = &[
    "__webpack_require__",
    "__webpack_exports__",
    "__webpack_modules__",
    "__webpack_module_cache__",
    "webpackChunk",
    "webpackJsonp",
];

/// Canonical esbuild runtime helper names. Each is declared at the top of
/// esbuild bundles whether or not it is referenced; the lowering pass strips
/// the declaration once we can prove the helper is unused.
pub(crate) const ESBUILD_RUNTIME_HELPERS: &[&str] = &[
    "__commonJS",
    "__toCommonJS",
    "__defProp",
    "__defProps",
    "__export",
    "__copyProps",
    "__toESM",
    "__require",
    "__esm",
    "__getOwnPropDesc",
    "__getOwnPropNames",
    "__hasOwnProp",
];

/// Catalog of Babel CJS interop helpers we know how to lower. Each entry
/// names the helper, the legal argument-count window for its call site, and
/// the source-level replacement template used when rewriting a
/// `var X = HELPER(require("Y"))` declarator init.
pub(crate) struct BabelInteropHelper {
    pub name: &'static str,
    pub min_call_args: usize,
    pub max_call_args: usize,
    pub replacement: fn(&str) -> String,
}

fn replace_with_default_wrapped_require(require_arg: &str) -> String {
    format!("{{ default: require({require_arg}) }}")
}

fn replace_with_bare_require(require_arg: &str) -> String {
    format!("require({require_arg})")
}

pub(crate) const BABEL_INTEROP_HELPERS: &[BabelInteropHelper] = &[
    BabelInteropHelper {
        name: "_interopRequireDefault",
        min_call_args: 1,
        max_call_args: 1,
        replacement: replace_with_default_wrapped_require,
    },
    BabelInteropHelper {
        name: "_interopRequireWildcard",
        min_call_args: 1,
        max_call_args: 2,
        replacement: replace_with_bare_require,
    },
];

/// Apply compiler-specific source-level rewrites before the main format pass.
/// Currently handles Babel's `_interopRequireDefault(require("X"))` and
/// `_interopRequireWildcard(require("X"))` patterns: the helper call is
/// dropped, leaving a literal `{ default: require(...) }` or a bare
/// `require(...)` respectively. Both keep downstream `.<member>` access
/// valid.
pub(crate) fn apply_source_level_lowerings(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
    lowering: CompilerLowering,
) -> String {
    let lowered = apply_babel_source_level_lowerings(source, path_hint, goal, lowering);
    inline_single_use_tiny_return_helpers(lowered.as_str(), path_hint, goal)
}

fn apply_babel_source_level_lowerings(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
    lowering: CompilerLowering,
) -> String {
    if !matches!(lowering, CompilerLowering::Babel) {
        return source.to_string();
    }
    let allocator = Allocator::default();
    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            continue;
        }
        let mut replacements = Vec::<(u32, u32, String)>::new();
        for statement in &parsed.program.body {
            let Statement::VariableDeclaration(declaration) = statement else {
                continue;
            };
            for declarator in &declaration.declarations {
                let Some(init) = declarator.init.as_ref() else {
                    continue;
                };
                for helper in BABEL_INTEROP_HELPERS {
                    let Some(arg_span) = babel_interop_helper_arg_span(init, helper) else {
                        continue;
                    };
                    let init_span = init.span();
                    let arg_text = &source[arg_span.start as usize..arg_span.end as usize];
                    replacements.push((
                        init_span.start,
                        init_span.end,
                        (helper.replacement)(arg_text),
                    ));
                    break;
                }
            }
        }
        if replacements.is_empty() {
            return source.to_string();
        }
        replacements.sort_by_key(|edit| edit.0);
        let mut output = source.to_string();
        for (start, end, new_text) in replacements.iter().rev() {
            output.replace_range(*start as usize..*end as usize, new_text);
        }
        return output;
    }
    // No source type parsed cleanly. Defer the parse-failure surface to the
    // downstream format pass.
    source.to_string()
}

#[derive(Debug, Clone)]
struct TinyReturnHelperCandidate {
    name: String,
    params: Vec<String>,
    enhanced: bool,
    declaration_span: Span,
    return_expression_span: Span,
}

const TINY_RETURN_HELPER_MAX_DECL_BYTES: u32 = 120;

fn inline_single_use_tiny_return_helpers(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> String {
    let allocator = Allocator::default();
    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            continue;
        }

        let allow_enhanced = allow_enhanced_tiny_return_inlining(path_hint);
        let mut candidates = BTreeMap::<String, TinyReturnHelperCandidate>::new();
        for statement in &parsed.program.body {
            let Some(candidate) = tiny_return_helper_candidate(statement, source) else {
                continue;
            };
            if candidate.enhanced && !allow_enhanced {
                continue;
            }
            candidates
                .entry(candidate.name.clone())
                .or_insert(candidate);
        }
        if candidates.is_empty() {
            return source.to_string();
        }

        let candidate_arities = candidates
            .iter()
            .map(|(name, candidate)| (name.clone(), candidate.params.len()))
            .collect::<BTreeMap<_, _>>();
        let mut uses = TinyReturnHelperUseCollector {
            candidates: &candidate_arities,
            total_refs: BTreeMap::new(),
            call_spans: BTreeMap::new(),
        };
        uses.visit_program(&parsed.program);

        let initially_selected = candidates
            .into_iter()
            .filter_map(|(name, candidate)| {
                let total_refs = uses.total_refs.get(name.as_str()).copied().unwrap_or(0);
                let call_spans = uses.call_spans.get(name.as_str())?;
                (total_refs == 1 && call_spans.len() == 1).then(|| (candidate, call_spans[0]))
            })
            .collect::<Vec<_>>();
        if initially_selected.is_empty() {
            return source.to_string();
        }

        let removal_spans = initially_selected
            .iter()
            .map(|(candidate, _)| candidate.declaration_span)
            .collect::<Vec<_>>();
        let mut edits = Vec::<(u32, u32, String)>::new();
        for (candidate, call_span) in initially_selected {
            // Avoid overlapping edits: if the only call sits inside another
            // helper declaration that is itself being removed, leave this
            // candidate alone for this pass. The emitted code stays correct;
            // we just skip a marginal dead-code cleanup opportunity.
            if removal_spans.iter().any(|span| {
                span.start != candidate.declaration_span.start
                    && span.start <= call_span.start
                    && call_span.end <= span.end
            }) {
                continue;
            }
            let Some(expression) =
                tiny_return_inlined_expression_source(source, &candidate, call_span)
            else {
                continue;
            };
            edits.push((
                candidate.declaration_span.start,
                candidate.declaration_span.end,
                String::new(),
            ));
            edits.push((call_span.start, call_span.end, format!("({expression})")));
        }
        if edits.is_empty() {
            return source.to_string();
        }
        edits.sort_by_key(|edit| edit.0);
        let mut output = source.to_string();
        for (start, end, replacement) in edits.iter().rev() {
            output.replace_range(*start as usize..*end as usize, replacement);
        }
        return output;
    }

    source.to_string()
}

fn tiny_return_helper_candidate(
    statement: &Statement<'_>,
    source: &str,
) -> Option<TinyReturnHelperCandidate> {
    if statement.span().size() > TINY_RETURN_HELPER_MAX_DECL_BYTES {
        return None;
    }
    let (name, params, expression) = match statement {
        Statement::FunctionDeclaration(function) => {
            if function.r#async || function.generator {
                return None;
            }
            (
                function.id.as_ref()?.name.as_str().to_string(),
                tiny_return_parameter_names(&function.params)?,
                function_return_expression(function)?,
            )
        }
        Statement::VariableDeclaration(declaration) => {
            if declaration.declarations.len() != 1 {
                return None;
            }
            let declarator = &declaration.declarations[0];
            let BindingPatternKind::BindingIdentifier(identifier) = &declarator.id.kind else {
                return None;
            };
            let init = declarator.init.as_ref()?;
            match init {
                Expression::ArrowFunctionExpression(arrow) => {
                    if arrow.r#async {
                        return None;
                    }
                    (
                        identifier.name.as_str().to_string(),
                        tiny_return_parameter_names(&arrow.params)?,
                        arrow_return_expression(arrow)?,
                    )
                }
                Expression::FunctionExpression(function) => {
                    if function.r#async || function.generator {
                        return None;
                    }
                    (
                        identifier.name.as_str().to_string(),
                        tiny_return_parameter_names(&function.params)?,
                        function_return_expression(function)?,
                    )
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    if params.len() > 1 {
        return None;
    }
    if !tiny_return_expression_is_safe(expression) {
        return None;
    }
    if !params.is_empty() && tiny_return_expression_blocks_parameter_substitution(expression) {
        return None;
    }
    let expression_span = expression.span();
    let expression_source = &source[expression_span.start as usize..expression_span.end as usize];
    if expression_source.len() > TINY_RETURN_HELPER_MAX_DECL_BYTES as usize {
        return None;
    }
    let enhanced = !params.is_empty() || matches!(statement, Statement::VariableDeclaration(_));
    Some(TinyReturnHelperCandidate {
        name,
        params,
        enhanced,
        declaration_span: statement.span(),
        return_expression_span: expression_span,
    })
}

fn allow_enhanced_tiny_return_inlining(path_hint: Option<&Path>) -> bool {
    path_hint.is_some_and(|path| {
        let path = path.to_string_lossy();
        path.contains("modules/runtime/")
            || path.starts_with("modules/runtime/")
            || path.contains("modules\\runtime\\")
            || path.starts_with("modules\\runtime\\")
    })
}

fn tiny_return_parameter_names(params: &oxc_ast::ast::FormalParameters<'_>) -> Option<Vec<String>> {
    if params.rest.is_some() {
        return None;
    }
    let mut names = Vec::with_capacity(params.items.len());
    for param in &params.items {
        let BindingPatternKind::BindingIdentifier(identifier) = &param.pattern.kind else {
            return None;
        };
        names.push(identifier.name.as_str().to_string());
    }
    Some(names)
}

fn function_return_expression<'a>(function: &'a Function<'a>) -> Option<&'a Expression<'a>> {
    let body = function.body.as_ref()?;
    let [Statement::ReturnStatement(return_statement)] = body.statements.as_slice() else {
        return None;
    };
    return_statement.argument.as_ref()
}

fn arrow_return_expression<'a>(
    arrow: &'a ArrowFunctionExpression<'a>,
) -> Option<&'a Expression<'a>> {
    let [statement] = arrow.body.statements.as_slice() else {
        return None;
    };
    if arrow.expression {
        let Statement::ExpressionStatement(statement) = statement else {
            return None;
        };
        Some(&statement.expression)
    } else {
        let Statement::ReturnStatement(statement) = statement else {
            return None;
        };
        statement.argument.as_ref()
    }
}

fn tiny_return_inlined_expression_source(
    source: &str,
    candidate: &TinyReturnHelperCandidate,
    call_span: Span,
) -> Option<String> {
    let expression = &source[candidate.return_expression_span.start as usize
        ..candidate.return_expression_span.end as usize];
    let [] = candidate.params.as_slice() else {
        let [param] = candidate.params.as_slice() else {
            return None;
        };
        return tiny_return_substituted_expression_source(source, candidate, call_span, param);
    };
    Some(expression.to_string())
}

fn tiny_return_substituted_expression_source(
    source: &str,
    candidate: &TinyReturnHelperCandidate,
    call_span: Span,
    param: &str,
) -> Option<String> {
    let call_source = &source[call_span.start as usize..call_span.end as usize];
    let arg_source = single_call_argument_source(call_source)?;
    if !tiny_inline_argument_is_safe(arg_source) {
        return None;
    }
    let mut collector = ParameterReferenceSpanCollector {
        target: param,
        spans: Vec::new(),
    };
    let expression_source = &source[candidate.return_expression_span.start as usize
        ..candidate.return_expression_span.end as usize];
    let allocator = Allocator::default();
    let wrapped = format!("({expression_source});");
    let parsed = Parser::new(&allocator, wrapped.as_str(), SourceType::ts()).parse();
    if !parsed.errors.is_empty() || parsed.panicked {
        return None;
    }
    if let Some(Statement::ExpressionStatement(statement)) = parsed.program.body.first()
        && let Expression::ParenthesizedExpression(parenthesized) = &statement.expression
    {
        collector.visit_expression(&parenthesized.expression);
    }
    let mut output = expression_source.to_string();
    let replacement = format!("({arg_source})");
    for span in collector.spans.iter().rev() {
        let start = span.start as usize - 1; // account for the wrapper `(`
        let end = span.end as usize - 1;
        output.replace_range(start..end, replacement.as_str());
    }
    Some(output)
}

fn single_call_argument_source(call_source: &str) -> Option<&str> {
    let open = call_source.find('(')?;
    let close = call_source.rfind(')')?;
    let arg = call_source.get(open + 1..close)?.trim();
    (!arg.is_empty() && !arg.contains(',')).then_some(arg)
}

fn tiny_inline_argument_is_safe(source: &str) -> bool {
    let allocator = Allocator::default();
    let wrapped = format!("({source});");
    let parsed = Parser::new(&allocator, wrapped.as_str(), SourceType::ts()).parse();
    if !parsed.errors.is_empty() || parsed.panicked {
        return false;
    }
    let Some(Statement::ExpressionStatement(statement)) = parsed.program.body.first() else {
        return false;
    };
    let Expression::ParenthesizedExpression(parenthesized) = &statement.expression else {
        return false;
    };
    matches!(
        &parenthesized.expression,
        Expression::Identifier(_)
            | Expression::StringLiteral(_)
            | Expression::NumericLiteral(_)
            | Expression::BooleanLiteral(_)
            | Expression::NullLiteral(_)
            | Expression::BigIntLiteral(_)
            | Expression::RegExpLiteral(_)
    )
}

fn tiny_return_expression_blocks_parameter_substitution(expression: &Expression<'_>) -> bool {
    let mut visitor = ParameterSubstitutionBlocker { blocked: false };
    visitor.visit_expression(expression);
    visitor.blocked
}

struct ParameterSubstitutionBlocker {
    blocked: bool,
}

impl<'a> Visit<'a> for ParameterSubstitutionBlocker {
    fn visit_expression(&mut self, expression: &Expression<'a>) {
        if matches!(expression, Expression::ObjectExpression(_)) {
            self.blocked = true;
            return;
        }
        walk_expression(self, expression);
    }
}

struct ParameterReferenceSpanCollector<'a> {
    target: &'a str,
    spans: Vec<Span>,
}

impl<'a> Visit<'a> for ParameterReferenceSpanCollector<'_> {
    fn visit_identifier_reference(&mut self, identifier: &IdentifierReference<'a>) {
        if identifier.name.as_str() == self.target {
            self.spans.push(identifier.span());
        }
    }
}

fn tiny_return_expression_is_safe(expression: &Expression<'_>) -> bool {
    let mut safety = TinyReturnExpressionSafety { safe: true };
    safety.visit_expression(expression);
    safety.safe
}

struct TinyReturnExpressionSafety {
    safe: bool,
}

impl<'a> Visit<'a> for TinyReturnExpressionSafety {
    fn visit_expression(&mut self, expression: &Expression<'a>) {
        if !self.safe {
            return;
        }
        match expression {
            Expression::ThisExpression(_)
            | Expression::Super(_)
            | Expression::MetaProperty(_)
            | Expression::AwaitExpression(_)
            | Expression::YieldExpression(_)
            | Expression::FunctionExpression(_)
            | Expression::ArrowFunctionExpression(_)
            | Expression::ClassExpression(_) => {
                self.safe = false;
                return;
            }
            _ => {}
        }
        walk_expression(self, expression);
    }

    fn visit_identifier_reference(&mut self, identifier: &IdentifierReference<'a>) {
        if identifier.name.as_str() == "arguments" {
            self.safe = false;
        }
    }
}

struct TinyReturnHelperUseCollector<'a> {
    candidates: &'a BTreeMap<String, usize>,
    total_refs: BTreeMap<String, u32>,
    call_spans: BTreeMap<String, Vec<Span>>,
}

impl<'a> Visit<'a> for TinyReturnHelperUseCollector<'_> {
    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if let Expression::Identifier(callee) = &call.callee
            && self
                .candidates
                .get(callee.name.as_str())
                .is_some_and(|arity| *arity == call.arguments.len())
        {
            self.call_spans
                .entry(callee.name.as_str().to_string())
                .or_default()
                .push(call.span());
        }
        walk_call_expression(self, call);
    }

    fn visit_identifier_reference(&mut self, identifier: &IdentifierReference<'a>) {
        let name = identifier.name.as_str();
        if self.candidates.contains_key(name) {
            *self.total_refs.entry(name.to_string()).or_insert(0) += 1;
        }
    }

    fn visit_export_named_declaration(&mut self, declaration: &ExportNamedDeclaration<'a>) {
        if declaration.source.is_none() {
            for specifier in &declaration.specifiers {
                if let Some(local) = module_export_name_text(&specifier.local)
                    && self.candidates.contains_key(local.as_str())
                {
                    *self.total_refs.entry(local).or_insert(0) += 1;
                }
            }
        }
        walk_export_named_declaration(self, declaration);
    }
}

/// Recognise the canonical Babel interop wrapper `HELPER(require("X"))` for
/// the supplied `helper` entry. Returns the span of the inner string-literal
/// specifier so the caller can re-use the original quoting and escaping.
fn babel_interop_helper_arg_span(
    expression: &Expression<'_>,
    helper: &BabelInteropHelper,
) -> Option<Span> {
    let Expression::CallExpression(outer) = expression else {
        return None;
    };
    let Expression::Identifier(callee) = &outer.callee else {
        return None;
    };
    if callee.name.as_str() != helper.name
        || outer.arguments.len() < helper.min_call_args
        || outer.arguments.len() > helper.max_call_args
    {
        return None;
    }
    let Argument::CallExpression(inner) = &outer.arguments[0] else {
        return None;
    };
    let Expression::Identifier(inner_callee) = &inner.callee else {
        return None;
    };
    if inner_callee.name.as_str() != "require" || inner.arguments.len() != 1 {
        return None;
    }
    let Argument::StringLiteral(specifier) = &inner.arguments[0] else {
        return None;
    };
    Some(specifier.span())
}

/// Returns true if any expression in the program refers to `name` as an
/// identifier (i.e. uses it as a value). Function declarations' own
/// `BindingIdentifier` does NOT show up as an `IdentifierReference`, so when
/// the count is zero the matching helper declaration is genuinely dead.
pub(crate) fn program_references_named_identifier(program: &Program<'_>, name: &str) -> bool {
    let mut counter = NamedIdentifierReferenceCounter {
        target: name,
        count: 0,
    };
    counter.visit_program(program);
    counter.count > 0
}

struct NamedIdentifierReferenceCounter<'name> {
    target: &'name str,
    count: usize,
}

impl<'a, 'name> Visit<'a> for NamedIdentifierReferenceCounter<'name> {
    fn visit_identifier_reference(&mut self, identifier: &IdentifierReference<'a>) {
        if identifier.name.as_str() == self.target {
            self.count += 1;
        }
    }
}

/// Recognise a Babel interop helper declaration that we can strip once dead.
/// The strict-name + non-empty-params + single-return-statement check stays
/// narrow enough that a user-written function with the same name but a
/// different shape is left alone.
pub(crate) fn is_babel_interop_helper_definition(
    statement: &Statement<'_>,
    helper: &BabelInteropHelper,
) -> bool {
    let Statement::FunctionDeclaration(function) = statement else {
        return false;
    };
    let Some(name) = function.id.as_ref() else {
        return false;
    };
    if name.name.as_str() != helper.name {
        return false;
    }
    if function.params.items.is_empty() {
        return false;
    }
    let Some(body) = function.body.as_ref() else {
        return false;
    };
    body.statements.len() == 1 && matches!(body.statements[0], Statement::ReturnStatement(_))
}

/// Recognise a top-level `var <name> = ...;` declaration (one declarator)
/// whose binding identifier matches `target_name`. Used by the esbuild
/// helper-strip pass — the helper bodies are too varied to match
/// structurally, so we identify them solely by their canonical names.
fn is_top_level_var_declaration_named(statement: &Statement<'_>, target_name: &str) -> bool {
    let Statement::VariableDeclaration(declaration) = statement else {
        return false;
    };
    if declaration.declarations.len() != 1 {
        return false;
    }
    let declarator = &declaration.declarations[0];
    let BindingPatternKind::BindingIdentifier(identifier) = &declarator.id.kind else {
        return false;
    };
    identifier.name.as_str() == target_name && declarator.init.is_some()
}

/// Recognise a top-level `function NAME(...)` declaration whose name matches
/// `target_name`. Used by the webpack helper-strip pass for helpers that
/// webpack emits as function declarations rather than var initializers
/// (e.g. `function __webpack_require__(id) { ... }`).
fn is_top_level_function_declaration_named(statement: &Statement<'_>, target_name: &str) -> bool {
    let Statement::FunctionDeclaration(function) = statement else {
        return false;
    };
    function
        .id
        .as_ref()
        .is_some_and(|id| id.name.as_str() == target_name)
}

/// Strip `var <name> = ...` declarations matching `helper_names` from both
/// the program top-level and the body of any top-level IIFE. Real esbuild
/// bundles wrap the runtime in `(() => { ... })()`, so module-scope strip
/// alone never fires on them; descending into the IIFE handles that case.
pub(crate) fn strip_named_var_declarations_in_program(
    program: &mut Program<'_>,
    helper_names: &[&str],
) {
    program.body.retain(|statement| {
        !helper_names
            .iter()
            .any(|name| is_top_level_var_declaration_named(statement, name))
    });
    for statement in program.body.iter_mut() {
        let Some(iife_body) = top_level_iife_body_statements_mut(statement) else {
            continue;
        };
        iife_body.retain(|statement| {
            !helper_names
                .iter()
                .any(|name| is_top_level_var_declaration_named(statement, name))
        });
    }
}

/// Strip `var <name> = ...` AND `function <name>(...) { ... }` declarations
/// matching `helper_names` from program top-level and any top-level IIFE
/// body. Webpack helpers come in both shapes (`__webpack_modules__` is a var,
/// `__webpack_require__` is a function declaration).
pub(crate) fn strip_named_declarations_in_program(
    program: &mut Program<'_>,
    helper_names: &[&str],
) {
    let matches_helper = |statement: &Statement<'_>| {
        helper_names.iter().any(|name| {
            is_top_level_var_declaration_named(statement, name)
                || is_top_level_function_declaration_named(statement, name)
        })
    };
    program.body.retain(|statement| !matches_helper(statement));
    for statement in program.body.iter_mut() {
        let Some(iife_body) = top_level_iife_body_statements_mut(statement) else {
            continue;
        };
        iife_body.retain(|statement| !matches_helper(statement));
    }
}

/// Return a mutable reference to the body statements of a top-level IIFE
/// expressed as `(() => { ... })()` or `(function () { ... })()`. The IIFE
/// must take no arguments — the arg-bearing webpack module-table form
/// (`(function(modules){...})([...])`) is intentionally excluded so we never
/// strip helpers from a parameterised module table.
fn top_level_iife_body_statements_mut<'a, 'p>(
    statement: &'p mut Statement<'a>,
) -> Option<&'p mut oxc_allocator::Vec<'a, Statement<'a>>> {
    let Statement::ExpressionStatement(expression_statement) = statement else {
        return None;
    };
    let Expression::CallExpression(call) = &mut expression_statement.expression else {
        return None;
    };
    if !call.arguments.is_empty() {
        return None;
    }
    let callee = unwrap_parenthesized_mut(&mut call.callee);
    match callee {
        Expression::ArrowFunctionExpression(arrow) => Some(&mut arrow.body.statements),
        Expression::FunctionExpression(function) => {
            function.body.as_mut().map(|body| &mut body.statements)
        }
        _ => None,
    }
}

fn unwrap_parenthesized_mut<'a, 'p>(
    mut expression: &'p mut Expression<'a>,
) -> &'p mut Expression<'a> {
    while let Expression::ParenthesizedExpression(parenthesized) = expression {
        expression = &mut parenthesized.expression;
    }
    expression
}

/// Strip webpack's `__webpack_require__.r(exports)` make-namespace marker
/// from program top-level and any top-level IIFE body. The call sets
/// `__esModule` on its argument; in an ESM emit context it is at best a
/// no-op and at worst a runtime ReferenceError, so dropping it always
/// improves output.
pub(crate) fn strip_webpack_make_namespace_markers_in_program(program: &mut Program<'_>) {
    program
        .body
        .retain(|statement| !is_webpack_make_namespace_marker(statement));
    for statement in program.body.iter_mut() {
        let Some(iife_body) = top_level_iife_body_statements_mut(statement) else {
            continue;
        };
        iife_body.retain(|statement| !is_webpack_make_namespace_marker(statement));
    }
}

/// Match a top-level `__webpack_require__.r(<single-arg>)` expression
/// statement. The argument may be any identifier (`exports`,
/// `__webpack_exports__`, etc.) since webpack picks the binding name from
/// the wrapping module function's parameter list.
fn is_webpack_make_namespace_marker(statement: &Statement<'_>) -> bool {
    let Statement::ExpressionStatement(statement) = statement else {
        return false;
    };
    let Expression::CallExpression(call) = &statement.expression else {
        return false;
    };
    let Expression::StaticMemberExpression(callee) = &call.callee else {
        return false;
    };
    let Expression::Identifier(callee_object) = &callee.object else {
        return false;
    };
    callee_object.name.as_str() == "__webpack_require__"
        && callee.property.name.as_str() == "r"
        && call.arguments.len() == 1
        && matches!(call.arguments[0], Argument::Identifier(_))
}

/// Recognise the canonical Babel CJS-to-ESM marker statement:
/// `Object.defineProperty(exports, "__esModule", { value: true });`. Stripping
/// it from emitted output is safe in an ES module context, where `exports` is
/// not bound and the assignment would otherwise be a runtime error.
pub(crate) fn is_babel_es_module_marker(statement: &Statement<'_>) -> bool {
    let Statement::ExpressionStatement(statement) = statement else {
        return false;
    };
    let Expression::CallExpression(call) = &statement.expression else {
        return false;
    };
    let Expression::StaticMemberExpression(callee) = &call.callee else {
        return false;
    };
    let Expression::Identifier(callee_object) = &callee.object else {
        return false;
    };
    if callee_object.name.as_str() != "Object" || callee.property.name.as_str() != "defineProperty"
    {
        return false;
    }
    if call.arguments.len() != 3 {
        return false;
    }
    let Argument::Identifier(target) = &call.arguments[0] else {
        return false;
    };
    if target.name.as_str() != "exports" {
        return false;
    }
    let Argument::StringLiteral(key) = &call.arguments[1] else {
        return false;
    };
    key.value.as_str() == "__esModule"
}
