use std::collections::BTreeSet;
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::{Argument, CallExpression, Expression, Statement};
use oxc_parser::Parser;

use crate::errors::ParseGoal;
use crate::parse::{parse_options_for, source_type_candidates};

/// AST-level body classifier for `lazyModule((exports, module) => { BODY })`
/// and `lazyValue(() => { BODY })` wrappers. Returns the source text of an
/// eager-safe value when the body matches a recognized shape (possibly
/// nested through `(function(){...}).call(...)` / `(()=>{...})()` IIFE
/// wrappers, and tolerating harmless `var`/`let`/`const` declarations
/// alongside the actual exports write):
///   * `module.exports = PURE_EXPR`
///   * `module.exports = A = B = PURE_EXPR` (chain — rightmost pure wins)
///   * `exports.k = PURE_EXPR_k` series → collapsed to `{ k1: v1, ... }`
///   * `Object.defineProperty(exports, "k", { value: PURE_EXPR })`
///   * `return PURE_EXPR;` (for `lazyValue` bodies)
///
/// Returns `None` for any unrecognized statement OR when the body has
/// thunk-call dependencies that need inter-procedural fixpoint
/// resolution. The richer [`classify_lazy_module_body`] is the
/// recommended entry point — callers that need the deps for fixpoint
/// propagation use it; callers that want a value-or-nothing answer
/// use this wrapper.
#[must_use]
pub fn extract_lazy_module_eager_value(
    body: &str,
    exports_param: &str,
    module_param: Option<&str>,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Option<String> {
    match classify_lazy_module_body(body, exports_param, module_param, path_hint, goal) {
        LazyBodyClassification::Eager { value } => Some(value),
        _ => None,
    }
}

/// Outcome of analyzing a lazy thunk body. The eagerification pipeline
/// uses this to decide whether the body can be inlined at module load,
/// and — when it depends on other thunks — what those dependencies are
/// so an inter-procedural fixpoint can resolve them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LazyBodyClassification {
    /// Body is mechanically eagerifiable with no calls into other
    /// lazy thunks. The `value` is the source-text to use as the
    /// replacement RHS (already includes any setter-call prologue
    /// folded into a comma expression).
    Eager { value: String },
    /// Body would be eagerifiable IF every name in `call_deps` resolves
    /// to a thunk that is itself eager-safe. The producer's fixpoint
    /// validates this by recursive propagation. The `value` is the
    /// composed replacement RHS assuming all deps clear; thunk calls
    /// like `dep()` are intentionally NOT in the prologue, because
    /// eagerifying each dep makes its side effects run at the dep
    /// module's load time (earlier than the consumer's), so re-calling
    /// would be redundant or unsafe.
    EagerWithDeps {
        value: String,
        call_deps: BTreeSet<String>,
    },
    /// Body has unrecognized side effects. Cannot be eagerified
    /// regardless of caller's eagerness.
    Impure,
}

/// Same as [`extract_lazy_module_eager_value`] but also accepts bodies
/// whose dependencies are all in `eager_safe_call_targets`. Returns the
/// composed value (with the dep calls already DROPPED from the prologue
/// — the eagerified producers will have already run by module-load time).
#[must_use]
pub fn extract_lazy_module_eager_value_with_safe_deps(
    body: &str,
    exports_param: &str,
    module_param: Option<&str>,
    path_hint: Option<&Path>,
    goal: ParseGoal,
    eager_safe_call_targets: &BTreeSet<String>,
) -> Option<String> {
    match classify_lazy_module_body(body, exports_param, module_param, path_hint, goal) {
        LazyBodyClassification::Eager { value } => Some(value),
        LazyBodyClassification::EagerWithDeps { value, call_deps } => {
            if call_deps
                .iter()
                .all(|d| eager_safe_call_targets.contains(d))
            {
                Some(value)
            } else {
                None
            }
        }
        LazyBodyClassification::Impure => None,
    }
}

/// Inter-procedural-friendly body classifier. Same shape recognition as
/// [`extract_lazy_module_eager_value`] but also reports zero-arg calls
/// to bare identifiers as dependencies for a global fixpoint to resolve.
/// The fixpoint determines whether each dependency identifier maps to a
/// thunk that is itself eager-safe; if so, the body is eager-safe.
#[must_use]
pub fn classify_lazy_module_body(
    body: &str,
    exports_param: &str,
    module_param: Option<&str>,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> LazyBodyClassification {
    let allocator = Allocator::default();
    let wrapped = format!("function __lazy_body_classifier_wrapper() {{\n{body}\n}}");
    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, &wrapped, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            continue;
        }
        let Some(function_body) = parsed.program.body.first().and_then(|stmt| match stmt {
            Statement::FunctionDeclaration(function) => function.body.as_deref(),
            _ => None,
        }) else {
            continue;
        };
        let analysis = analyze_lazy_body_statements(
            &function_body.statements,
            &wrapped,
            exports_param,
            module_param,
        );
        return analysis_to_classification(analysis, module_param);
    }
    LazyBodyClassification::Impure
}

/// Internal mutable state collected during AST traversal of a lazy
/// body. The analyzer fills this in; `analysis_to_classification` converts it
/// to a `LazyBodyClassification` based on whether dependencies were collected.
#[derive(Debug, Default)]
struct LazyBodyAnalysisState {
    captured_value: Option<String>,
    property_writes: Vec<(String, String)>,
    prologue: Vec<String>,
    call_deps: BTreeSet<String>,
    impure: bool,
}

impl LazyBodyAnalysisState {
    fn push_property_write(&mut self, key: String, value: String) -> bool {
        if self
            .property_writes
            .iter()
            .any(|(existing_key, _)| existing_key == &key)
        {
            return false;
        }
        self.property_writes.push((key, value));
        true
    }

    fn extend_property_writes(&mut self, writes: Vec<(String, String)>) -> bool {
        for (key, value) in writes {
            if !self.push_property_write(key, value) {
                return false;
            }
        }
        true
    }
}

fn analyze_lazy_body_statements(
    statements: &oxc_allocator::Vec<'_, Statement<'_>>,
    source: &str,
    exports_param: &str,
    module_param: Option<&str>,
) -> LazyBodyAnalysisState {
    let mut state = LazyBodyAnalysisState::default();
    for stmt in statements {
        if state.impure {
            break;
        }
        match stmt {
            Statement::VariableDeclaration(decl) => {
                if !is_harmless_variable_declaration(decl, source) {
                    state.impure = true;
                }
            }
            Statement::EmptyStatement(_) => {}
            Statement::ExpressionStatement(expr_stmt) => {
                let mut chain = &expr_stmt.expression;
                while let Expression::ParenthesizedExpression(inner) = chain {
                    chain = &inner.expression;
                }
                if let Some(inner_body) = iife_block_body(chain) {
                    let inner_state = analyze_lazy_body_statements(
                        &inner_body.statements,
                        source,
                        exports_param,
                        module_param,
                    );
                    if inner_state.impure {
                        state.impure = true;
                        continue;
                    }
                    if state.captured_value.is_some() || !state.property_writes.is_empty() {
                        // Another exports write or value already captured —
                        // having two values from different statements is
                        // ambiguous. Refuse.
                        state.impure = true;
                        continue;
                    }
                    // Merge inner state into outer.
                    state.prologue.extend(inner_state.prologue);
                    state.call_deps.extend(inner_state.call_deps);
                    if let Some(value) = inner_state.captured_value {
                        state.captured_value = Some(value);
                    } else if !inner_state.property_writes.is_empty()
                        && !state.extend_property_writes(inner_state.property_writes)
                    {
                        state.impure = true;
                    }
                    // If inner had only prologue (init-only), the IIFE
                    // contributes no value but its prologue's side
                    // effects bubble up.
                    continue;
                }
                if let Expression::AssignmentExpression(assign) = chain {
                    if let Some(module_name) = module_param
                        && is_module_exports_target(&assign.left, module_name)
                    {
                        let final_value = unwrap_assignment_chain(&assign.right);
                        if !is_pure_eager_expression(final_value, source) {
                            state.impure = true;
                            continue;
                        }
                        if state.captured_value.is_some() || !state.property_writes.is_empty() {
                            state.impure = true;
                            continue;
                        }
                        state.captured_value = Some(span_text(final_value, source).to_string());
                        continue;
                    }
                    if let Some((key, value)) =
                        match_exports_key_assignment(assign, exports_param, source)
                    {
                        if state.captured_value.is_some() {
                            state.impure = true;
                            continue;
                        }
                        if !state.push_property_write(key, value) {
                            state.impure = true;
                        }
                        continue;
                    }
                    state.impure = true;
                    continue;
                }
                if let Expression::CallExpression(call) = chain {
                    if let Some((key, value)) =
                        match_object_define_property(call, exports_param, source)
                    {
                        if state.captured_value.is_some() {
                            state.impure = true;
                            continue;
                        }
                        if !state.push_property_write(key, value) {
                            state.impure = true;
                        }
                        continue;
                    }
                    if is_reverts_setter_call_with_pure_args(call, source) {
                        let inner: &CallExpression<'_> = call;
                        state.prologue.push(span_text(inner, source).to_string());
                        continue;
                    }
                    // A zero-arg call to a bare identifier is an
                    // inter-procedural dependency. The fixpoint determines
                    // whether the called binding is itself eager-safe; if all
                    // deps clear, the call's side effects are subsumed by the
                    // dep's eagerification (which runs at the dep's
                    // module-load time, before this module's). We do not add
                    // the call to the prologue — re-invoking an eagerified
                    // binding would dereference a non-function.
                    if call.arguments.is_empty()
                        && let Expression::Identifier(callee) = &call.callee
                    {
                        state.call_deps.insert(callee.name.to_string());
                        continue;
                    }
                    state.impure = true;
                    continue;
                }
                if matches!(chain, Expression::Identifier(_)) {
                    continue;
                }
                if let Expression::SequenceExpression(seq) = chain {
                    let mut all_acceptable = true;
                    for sub in &seq.expressions {
                        match sub {
                            Expression::Identifier(_) => {}
                            Expression::CallExpression(c) => {
                                if is_reverts_setter_call_with_pure_args(c, source) {
                                    let inner: &CallExpression<'_> = c;
                                    state.prologue.push(span_text(inner, source).to_string());
                                } else if c.arguments.is_empty() {
                                    if let Expression::Identifier(callee) = &c.callee {
                                        state.call_deps.insert(callee.name.to_string());
                                    } else {
                                        all_acceptable = false;
                                        break;
                                    }
                                } else {
                                    all_acceptable = false;
                                    break;
                                }
                            }
                            _ => {
                                all_acceptable = false;
                                break;
                            }
                        }
                    }
                    if !all_acceptable {
                        state.impure = true;
                    }
                    continue;
                }
                state.impure = true;
            }
            Statement::ReturnStatement(ret) => {
                let Some(arg) = &ret.argument else {
                    state.impure = true;
                    continue;
                };
                let final_value = unwrap_assignment_chain(arg);
                if !is_pure_eager_expression(final_value, source) {
                    state.impure = true;
                    continue;
                }
                if state.captured_value.is_some() || !state.property_writes.is_empty() {
                    state.impure = true;
                    continue;
                }
                state.captured_value = Some(span_text(final_value, source).to_string());
            }
            _ => {
                state.impure = true;
            }
        }
    }
    state
}

fn analysis_to_classification(
    state: LazyBodyAnalysisState,
    module_param: Option<&str>,
) -> LazyBodyClassification {
    if state.impure {
        return LazyBodyClassification::Impure;
    }
    let base_value: Option<String> = if let Some(value) = state.captured_value {
        Some(value)
    } else if !state.property_writes.is_empty() {
        let formatted = state
            .property_writes
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!("{{ {formatted} }}"))
    } else if !state.prologue.is_empty() || !state.call_deps.is_empty() {
        Some(if module_param.is_some() {
            "{}".into()
        } else {
            "void 0".into()
        })
    } else {
        None
    };
    let Some(base) = base_value else {
        return LazyBodyClassification::Impure;
    };
    let value = if state.prologue.is_empty() {
        base
    } else {
        let mut combined = String::new();
        combined.push('(');
        for stmt in &state.prologue {
            combined.push_str(stmt);
            combined.push_str(", ");
        }
        combined.push_str(&base);
        combined.push(')');
        combined
    };
    if state.call_deps.is_empty() {
        LazyBodyClassification::Eager { value }
    } else {
        LazyBodyClassification::EagerWithDeps {
            value,
            call_deps: state.call_deps,
        }
    }
}

fn is_reverts_setter_call_with_pure_args(call: &CallExpression<'_>, source: &str) -> bool {
    let Expression::Identifier(callee) = &call.callee else {
        return false;
    };
    if !callee.name.as_str().starts_with("__reverts_set_") {
        return false;
    }
    if call.arguments.len() != 1 {
        return false;
    }
    is_pure_setter_argument(&call.arguments[0], source)
}

/// Same shape as `is_pure_eager_expression` but applied to the
/// `Argument` variants of OXC AST. The two enums share discriminants
/// (via OXC's `inherit_variants!` macro) but Rust treats them as
/// distinct types — there's no zero-cost `Argument -> Expression`
/// view in OXC 0.42, so we re-state the variant match here. Only the
/// shapes a reverts-emitted setter call ever receives (literal-like
/// values, function/class expressions, simple unary negations) are
/// accepted; anything else (call, member access, identifier reference)
/// keeps the lazy thunk to be safe.
fn is_pure_setter_argument(arg: &Argument<'_>, source: &str) -> bool {
    use oxc_ast::ast::Argument as A;
    match arg {
        A::NumericLiteral(_)
        | A::StringLiteral(_)
        | A::BooleanLiteral(_)
        | A::NullLiteral(_)
        | A::BigIntLiteral(_)
        | A::RegExpLiteral(_)
        | A::TemplateLiteral(_)
        | A::FunctionExpression(_)
        | A::ArrowFunctionExpression(_)
        | A::ClassExpression(_) => true,
        A::ObjectExpression(obj) => is_pure_object_expression(obj, source),
        A::ArrayExpression(arr) => arr
            .elements
            .iter()
            .all(|element| is_pure_array_element(element, source)),
        A::ParenthesizedExpression(inner) => is_pure_eager_expression(&inner.expression, source),
        A::UnaryExpression(unary) => {
            matches!(
                unary.operator,
                oxc_syntax::operator::UnaryOperator::LogicalNot
                    | oxc_syntax::operator::UnaryOperator::UnaryNegation
                    | oxc_syntax::operator::UnaryOperator::UnaryPlus
                    | oxc_syntax::operator::UnaryOperator::BitwiseNot
                    | oxc_syntax::operator::UnaryOperator::Void
            ) && is_pure_eager_expression(&unary.argument, source)
        }
        _ => false,
    }
}

fn is_harmless_variable_declaration(
    decl: &oxc_ast::ast::VariableDeclaration<'_>,
    source: &str,
) -> bool {
    decl.declarations
        .iter()
        .all(|declarator| match &declarator.init {
            None => true,
            Some(init) => is_pure_eager_expression(init, source),
        })
}

fn iife_block_body<'a>(expr: &'a Expression<'a>) -> Option<&'a oxc_ast::ast::FunctionBody<'a>> {
    let Expression::CallExpression(call) = expr else {
        return None;
    };
    if let Some(body) = function_body_of_invokable(&call.callee) {
        return Some(body);
    }
    if let Expression::StaticMemberExpression(member) = &call.callee {
        let prop = member.property.name.as_str();
        if (prop == "call" || prop == "apply")
            && let Some(body) = function_body_of_invokable(&member.object)
        {
            return Some(body);
        }
    }
    None
}

fn function_body_of_invokable<'a>(
    expr: &'a Expression<'a>,
) -> Option<&'a oxc_ast::ast::FunctionBody<'a>> {
    match expr {
        Expression::ParenthesizedExpression(inner) => function_body_of_invokable(&inner.expression),
        Expression::FunctionExpression(function) => function.body.as_deref(),
        Expression::ArrowFunctionExpression(arrow) if !arrow.expression => Some(&arrow.body),
        _ => None,
    }
}

fn is_module_exports_target(
    target: &oxc_ast::ast::AssignmentTarget<'_>,
    module_param: &str,
) -> bool {
    let oxc_ast::ast::AssignmentTarget::StaticMemberExpression(member) = target else {
        return false;
    };
    let Expression::Identifier(object) = &member.object else {
        return false;
    };
    object.name.as_str() == module_param && member.property.name.as_str() == "exports"
}

fn match_exports_key_assignment(
    assign: &oxc_ast::ast::AssignmentExpression<'_>,
    exports_param: &str,
    source: &str,
) -> Option<(String, String)> {
    let oxc_ast::ast::AssignmentTarget::StaticMemberExpression(member) = &assign.left else {
        return None;
    };
    let Expression::Identifier(object) = &member.object else {
        return None;
    };
    if object.name.as_str() != exports_param {
        return None;
    }
    let key = member.property.name.as_str().to_string();
    let final_value = unwrap_assignment_chain(&assign.right);
    if !is_pure_eager_expression(final_value, source) {
        return None;
    }
    Some((key, span_text(final_value, source).to_string()))
}

fn match_object_define_property(
    call: &oxc_ast::ast::CallExpression<'_>,
    exports_param: &str,
    source: &str,
) -> Option<(String, String)> {
    let Expression::StaticMemberExpression(callee) = &call.callee else {
        return None;
    };
    let Expression::Identifier(object_name) = &callee.object else {
        return None;
    };
    if object_name.name.as_str() != "Object" || callee.property.name.as_str() != "defineProperty" {
        return None;
    }
    if call.arguments.len() < 3 {
        return None;
    }
    let Argument::Identifier(target) = &call.arguments[0] else {
        return None;
    };
    if target.name.as_str() != exports_param {
        return None;
    }
    let key = match &call.arguments[1] {
        Argument::StringLiteral(s) => s.value.as_str().to_string(),
        _ => return None,
    };
    let Argument::ObjectExpression(descriptor) = &call.arguments[2] else {
        return None;
    };
    let mut value_text: Option<String> = None;
    for prop in &descriptor.properties {
        let oxc_ast::ast::ObjectPropertyKind::ObjectProperty(property) = prop else {
            return None;
        };
        let oxc_ast::ast::PropertyKey::StaticIdentifier(prop_name) = &property.key else {
            return None;
        };
        match prop_name.name.as_str() {
            "value" => {
                if !is_pure_eager_expression(&property.value, source) {
                    return None;
                }
                value_text = Some(span_text(&property.value, source).to_string());
            }
            "writable" | "configurable" | "enumerable" => {}
            _ => return None,
        }
    }
    Some((key, value_text?))
}

fn unwrap_assignment_chain<'a>(expr: &'a Expression<'a>) -> &'a Expression<'a> {
    match expr {
        Expression::AssignmentExpression(assign) => unwrap_assignment_chain(&assign.right),
        Expression::ParenthesizedExpression(inner) => unwrap_assignment_chain(&inner.expression),
        _ => expr,
    }
}

fn is_pure_eager_expression(expr: &Expression<'_>, source: &str) -> bool {
    match expr {
        Expression::NumericLiteral(_)
        | Expression::StringLiteral(_)
        | Expression::BooleanLiteral(_)
        | Expression::NullLiteral(_)
        | Expression::BigIntLiteral(_)
        | Expression::RegExpLiteral(_)
        | Expression::TemplateLiteral(_)
        | Expression::FunctionExpression(_)
        | Expression::ArrowFunctionExpression(_)
        | Expression::ClassExpression(_) => true,
        Expression::ObjectExpression(obj) => is_pure_object_expression(obj, source),
        Expression::ArrayExpression(arr) => arr
            .elements
            .iter()
            .all(|element| is_pure_array_element(element, source)),
        Expression::ParenthesizedExpression(inner) => {
            is_pure_eager_expression(&inner.expression, source)
        }
        Expression::UnaryExpression(unary) => {
            matches!(
                unary.operator,
                oxc_syntax::operator::UnaryOperator::LogicalNot
                    | oxc_syntax::operator::UnaryOperator::UnaryNegation
                    | oxc_syntax::operator::UnaryOperator::UnaryPlus
                    | oxc_syntax::operator::UnaryOperator::BitwiseNot
                    | oxc_syntax::operator::UnaryOperator::Void
            ) && is_pure_eager_expression(&unary.argument, source)
        }
        _ => false,
    }
}

fn is_pure_object_expression(obj: &oxc_ast::ast::ObjectExpression<'_>, source: &str) -> bool {
    for prop in &obj.properties {
        match prop {
            oxc_ast::ast::ObjectPropertyKind::ObjectProperty(p) => {
                if p.computed {
                    return false;
                }
                if !is_pure_eager_expression(&p.value, source) {
                    return false;
                }
            }
            oxc_ast::ast::ObjectPropertyKind::SpreadProperty(_) => return false,
        }
    }
    true
}

fn is_pure_array_element(elem: &oxc_ast::ast::ArrayExpressionElement<'_>, source: &str) -> bool {
    // `ArrayExpressionElement` shares discriminants with `Expression`
    // via OXC's `inherit_variants!` macro but the two enums are distinct
    // types in Rust — there's no zero-cost `&ArrayExpressionElement →
    // &Expression` view. We re-state the same recursive shape match here
    // so that arrays of nested objects, arrays, and other pure shapes
    // are accepted (matching the byte-level `pure_array_literal` scanner
    // in the planner). The two enums share `Elision` and `SpreadElement`,
    // which `Expression` doesn't have — spread keeps the lazy thunk to
    // be safe.
    use oxc_ast::ast::ArrayExpressionElement as A;
    match elem {
        A::Elision(_)
        | A::NumericLiteral(_)
        | A::StringLiteral(_)
        | A::BooleanLiteral(_)
        | A::NullLiteral(_)
        | A::BigIntLiteral(_)
        | A::RegExpLiteral(_)
        | A::TemplateLiteral(_)
        | A::FunctionExpression(_)
        | A::ArrowFunctionExpression(_)
        | A::ClassExpression(_) => true,
        A::ObjectExpression(obj) => is_pure_object_expression(obj, source),
        A::ArrayExpression(arr) => arr
            .elements
            .iter()
            .all(|element| is_pure_array_element(element, source)),
        A::ParenthesizedExpression(inner) => is_pure_eager_expression(&inner.expression, source),
        A::UnaryExpression(unary) => {
            matches!(
                unary.operator,
                oxc_syntax::operator::UnaryOperator::LogicalNot
                    | oxc_syntax::operator::UnaryOperator::UnaryNegation
                    | oxc_syntax::operator::UnaryOperator::UnaryPlus
                    | oxc_syntax::operator::UnaryOperator::BitwiseNot
                    | oxc_syntax::operator::UnaryOperator::Void
            ) && is_pure_eager_expression(&unary.argument, source)
        }
        _ => false,
    }
}

fn span_text<'a>(node: &impl oxc_span::GetSpan, source: &'a str) -> &'a str {
    let span = node.span();
    &source[span.start as usize..span.end as usize]
}
