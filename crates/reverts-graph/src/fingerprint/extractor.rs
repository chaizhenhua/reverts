use oxc_allocator::Allocator;
use oxc_ast::Visit;
use oxc_ast::ast::{
    ArrowFunctionExpression, AssignmentExpression, AssignmentTarget, BindingPatternKind,
    Expression, FormalParameters, Function, FunctionBody, MethodDefinition, ObjectProperty,
    Program, PropertyDefinition, PropertyKey, VariableDeclarator,
};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};
use oxc_syntax::scope::ScopeFlags;
use reverts_ir::{AxisHashes, ByteRange, FunctionFingerprint, FunctionId, ModuleId};
use reverts_js::normalize::{apply_to_source, stable_passes};
use reverts_js::parse_options_for;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedFunction {
    pub id: FunctionId,
    pub param_count: u32,
    pub statement_count: u32,
    pub is_async: bool,
    pub is_generator: bool,
}

pub struct FunctionExtractor {
    module_id: ModuleId,
    out: Vec<ExtractedFunction>,
}

impl FunctionExtractor {
    #[must_use]
    pub fn new(module_id: ModuleId) -> Self {
        Self {
            module_id,
            out: Vec::new(),
        }
    }

    pub fn extract<'a>(mut self, program: &Program<'a>) -> Vec<ExtractedFunction> {
        self.visit_program(program);
        self.out
    }
}

impl<'a> Visit<'a> for FunctionExtractor {
    fn visit_function(&mut self, func: &Function<'a>, flags: ScopeFlags) {
        let span = func.span();
        let stmt_count = func.body.as_ref().map_or(0, |body| body.statements.len()) as u32;
        self.out.push(ExtractedFunction {
            id: FunctionId::new(self.module_id, ByteRange::new(span.start, span.end)),
            param_count: func.params.items.len() as u32,
            statement_count: stmt_count,
            is_async: func.r#async,
            is_generator: func.generator,
        });
        oxc_ast::visit::walk::walk_function(self, func, flags);
    }

    fn visit_arrow_function_expression(&mut self, arrow: &ArrowFunctionExpression<'a>) {
        let span = arrow.span();
        let stmt_count = arrow.body.statements.len() as u32;
        self.out.push(ExtractedFunction {
            id: FunctionId::new(self.module_id, ByteRange::new(span.start, span.end)),
            param_count: arrow.params.items.len() as u32,
            statement_count: stmt_count,
            is_async: arrow.r#async,
            is_generator: false,
        });
        oxc_ast::visit::walk::walk_arrow_function_expression(self, arrow);
    }
}

impl FunctionExtractor {
    /// Counts functions with the primary parser/extractor path only.
    ///
    /// This is intended for diagnostics where only a count is needed; unlike
    /// [`Self::fingerprint`], it does not run alternate normalization passes or
    /// compute per-axis hashes.
    #[must_use]
    pub fn function_count(module_id: ModuleId, source: &str) -> usize {
        let source = strip_outer_block_braces(source);
        let alloc = Allocator::default();
        let source_type = SourceType::default().with_typescript(true).with_jsx(true);
        let parsed = Parser::new(&alloc, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if parsed.panicked || !parsed.errors.is_empty() {
            return 0;
        }
        Self::new(module_id).extract(&parsed.program).len()
    }

    /// Computes per-function fingerprints with primary axes plus one alternate
    /// per normalization pass. Returns empty if the source fails to parse.
    ///
    /// Bundle-extracted module bodies arrive here as `{ stmt; stmt; }` —
    /// the braces are the original arrow-function body delimiters. OXC
    /// parses such input as a top-level `BlockStatement`, and the default
    /// `Visit` walker does NOT descend into block-nested
    /// `FunctionDeclaration`s the same way it descends into program-level
    /// ones. To keep the extractor blind to this packaging detail, we
    /// strip a single pair of outer braces before parsing.
    #[must_use]
    pub fn fingerprint(module_id: ModuleId, source: &str) -> Vec<FunctionFingerprint> {
        let source = strip_outer_block_braces(source);
        let alloc = Allocator::default();
        let source_type = SourceType::default().with_typescript(true).with_jsx(true);
        let parsed = Parser::new(&alloc, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if parsed.panicked || !parsed.errors.is_empty() {
            return Vec::new();
        }
        let primary_extracts = Self::new(module_id).extract(&parsed.program);
        let primary_locals = collect_universal_renamable_bindings(&parsed.program);

        let mut out: Vec<FunctionFingerprint> = primary_extracts
            .iter()
            .filter_map(|f| {
                let (params, body) = locate_function(&parsed.program, f.id.span)?;
                Some(FunctionFingerprint {
                    id: f.id,
                    param_count: f.param_count,
                    statement_count: f.statement_count,
                    primary: compute_axes(params, body, &primary_locals),
                    alternates: Vec::new(),
                })
            })
            .collect();

        for pass in stable_passes() {
            let Ok(transformed) = apply_to_source(pass.as_ref(), source) else {
                continue;
            };
            let alt_alloc = Allocator::default();
            let alt_parsed = Parser::new(&alt_alloc, &transformed, source_type)
                .with_options(parse_options_for(source_type))
                .parse();
            if alt_parsed.panicked || !alt_parsed.errors.is_empty() {
                continue;
            }
            let alt_extracts = Self::new(module_id).extract(&alt_parsed.program);
            let alt_locals = collect_universal_renamable_bindings(&alt_parsed.program);
            for (i, alt_fn) in alt_extracts.iter().enumerate() {
                let Some(fp) = out.get_mut(i) else {
                    break;
                };
                if fp.param_count != alt_fn.param_count {
                    continue;
                }
                let Some((params, body)) = locate_function(&alt_parsed.program, alt_fn.id.span)
                else {
                    continue;
                };
                fp.alternates.push(reverts_ir::AlternateAxisHashes {
                    pass: pass.id(),
                    statement_count: alt_fn.statement_count,
                    axes: compute_axes(params, body, &alt_locals),
                });
            }
        }
        out
    }
}

/// Map each function/arrow span to the name it is declared under, parsed from
/// the SAME stripped source that [`FunctionExtractor::fingerprint`] sees so the
/// spans line up with the returned [`FunctionId`]s. Only functions with a
/// recoverable name are included; anonymous callbacks / IIFEs are omitted.
///
/// On real source (the reference tree) this yields the human name
/// (`classifyForCollapse`); on emitted minified source it yields the minified
/// binding (`xY7`). Pairing the two by α-rename-invariant AST hash within a
/// matched module is what turns "module M ↔ file F" into per-function renames.
#[must_use]
pub fn function_names(source: &str) -> std::collections::BTreeMap<ByteRange, String> {
    let source = strip_outer_block_braces(source);
    let alloc = Allocator::default();
    let source_type = SourceType::default().with_typescript(true).with_jsx(true);
    let parsed = Parser::new(&alloc, source, source_type)
        .with_options(parse_options_for(source_type))
        .parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return std::collections::BTreeMap::new();
    }
    let mut extractor = FunctionNameExtractor {
        names: std::collections::BTreeMap::new(),
    };
    extractor.visit_program(&parsed.program);
    extractor.names
}

struct FunctionNameExtractor {
    names: std::collections::BTreeMap<ByteRange, String>,
}

impl FunctionNameExtractor {
    /// Record `name` for the function/arrow `expr` resolves to (peeling
    /// parentheses). First writer wins — an inner declaration name (e.g.
    /// `var x = function realName(){}`) is preferred over the outer binding.
    fn record(&mut self, expr: &Expression<'_>, name: &str) {
        if let Some(range) = function_like_span(expr) {
            self.names.entry(range).or_insert_with(|| name.to_string());
        }
    }
}

fn function_like_span(expr: &Expression<'_>) -> Option<ByteRange> {
    match expr {
        Expression::FunctionExpression(function) => {
            let span = function.span();
            Some(ByteRange::new(span.start, span.end))
        }
        Expression::ArrowFunctionExpression(arrow) => {
            let span = arrow.span();
            Some(ByteRange::new(span.start, span.end))
        }
        Expression::ParenthesizedExpression(paren) => function_like_span(&paren.expression),
        _ => None,
    }
}

fn property_key_name(key: &PropertyKey<'_>) -> Option<String> {
    match key {
        PropertyKey::StaticIdentifier(identifier) => Some(identifier.name.to_string()),
        PropertyKey::StringLiteral(literal) => Some(literal.value.to_string()),
        _ => None,
    }
}

/// Name an assignment LHS contributes: a bare identifier (`fn = …`) or the
/// trailing static member (`X.fn = …`, `X.prototype.fn = …` → `fn`).
fn assignment_target_name(target: &AssignmentTarget<'_>) -> Option<String> {
    match target {
        AssignmentTarget::AssignmentTargetIdentifier(identifier) => {
            Some(identifier.name.to_string())
        }
        AssignmentTarget::StaticMemberExpression(member) => Some(member.property.name.to_string()),
        _ => None,
    }
}

impl<'a> Visit<'a> for FunctionNameExtractor {
    fn visit_function(&mut self, func: &Function<'a>, flags: ScopeFlags) {
        if let Some(id) = &func.id {
            let span = func.span();
            self.names
                .entry(ByteRange::new(span.start, span.end))
                .or_insert_with(|| id.name.to_string());
        }
        oxc_ast::visit::walk::walk_function(self, func, flags);
    }

    fn visit_variable_declarator(&mut self, declarator: &VariableDeclarator<'a>) {
        if let BindingPatternKind::BindingIdentifier(id) = &declarator.id.kind
            && let Some(init) = &declarator.init
        {
            self.record(init, id.name.as_str());
        }
        oxc_ast::visit::walk::walk_variable_declarator(self, declarator);
    }

    fn visit_object_property(&mut self, property: &ObjectProperty<'a>) {
        if let Some(name) = property_key_name(&property.key) {
            self.record(&property.value, &name);
        }
        oxc_ast::visit::walk::walk_object_property(self, property);
    }

    fn visit_method_definition(&mut self, method: &MethodDefinition<'a>) {
        if let Some(name) = property_key_name(&method.key) {
            let span = method.value.span();
            self.names
                .entry(ByteRange::new(span.start, span.end))
                .or_insert(name);
        }
        oxc_ast::visit::walk::walk_method_definition(self, method);
    }

    fn visit_property_definition(&mut self, property: &PropertyDefinition<'a>) {
        if let (Some(name), Some(value)) = (property_key_name(&property.key), &property.value) {
            self.record(value, &name);
        }
        oxc_ast::visit::walk::walk_property_definition(self, property);
    }

    fn visit_assignment_expression(&mut self, assignment: &AssignmentExpression<'a>) {
        if let Some(name) = assignment_target_name(&assignment.left) {
            self.record(&assignment.right, &name);
        }
        oxc_ast::visit::walk::walk_assignment_expression(self, assignment);
    }
}

/// If `src` is exactly one outer pair of `{ ... }` (with only whitespace
/// before the opening brace and after the closing one), return the inner
/// slice. Otherwise return `src` unchanged. This unwraps block-statement
/// module sources produced by the bundle extractor so the OXC parser
/// sees a normal program-level sequence of statements.
fn strip_outer_block_braces(src: &str) -> &str {
    let trimmed = src.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') && trimmed.len() >= 2 {
        // Ensure the outer braces actually match (no early-close that
        // would make the inner slice invalid). Cheap parity check: scan
        // brace depth and confirm the first `{` only closes at the end.
        let bytes = trimmed.as_bytes();
        let mut depth: i32 = 0;
        for (i, &b) in bytes.iter().enumerate() {
            match b {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 && i != bytes.len() - 1 {
                        return src;
                    }
                }
                _ => {}
            }
        }
        if depth == 0 {
            return &trimmed[1..trimmed.len() - 1];
        }
    }
    src
}

fn compute_axes<'a>(
    params: &FormalParameters<'a>,
    body: &FunctionBody<'a>,
    program_locals: &std::collections::BTreeSet<&str>,
) -> AxisHashes {
    let (acc_p, acc_s) = super::access::compute(body);
    // Per-function scope: union program-level locals with this
    // function's params and body-level bindings. Calling a name in
    // this combined set is a call into renameable territory and gets
    // filtered out of callee_set.
    let mut function_locals = program_locals.clone();
    collect_function_scope_binding_names(params, body, &mut function_locals);
    AxisHashes {
        ast: super::ast::compute(body),
        cfg: super::cfg::compute(body),
        return_pattern: super::return_pattern::compute(body),
        effect_pattern: super::effect_pattern::compute(body),
        literal_anchor: super::literal_anchor::compute(body),
        access_pattern: acc_p,
        structural_anchor: super::structural_anchor::compute(params, body),
        literal_shape: super::literal_shape::compute(body),
        access_shape: acc_s,
        callee_set: super::callee_set::compute_with_locals(body, &function_locals),
        binding_pattern: super::binding_pattern::compute(params, body),
        throw_set: super::throw_set::compute_with_locals(body, &function_locals),
    }
}

/// Collect every identifier name bound anywhere within a function:
/// formal parameters, top-level body declarations, **and block-scope
/// let/const/var/function/class declarations inside if-bodies,
/// loops, try-catch handlers, and nested blocks**. Nested function
/// bodies are NOT recursed into — they have their own scope and will
/// be processed when their own `compute_axes` runs.
///
/// Why include block-scope bindings? Because a minifier rewrites them
/// the same way it rewrites function-scope locals — `if (cond) { let
/// helper = ...; helper(); }` becomes `if (cond) { let K = ...;
/// K(); }`. The name `K` is unstable across builds and we want to
/// filter it out of `callee_set` for the same reason we filter
/// function-scope locals.
fn collect_function_scope_binding_names<'b>(
    params: &'b FormalParameters<'_>,
    body: &'b FunctionBody<'_>,
    set: &mut std::collections::BTreeSet<&'b str>,
) {
    use oxc_ast::ast::{BindingPatternKind, Statement};
    fn visit_pattern<'b>(
        kind: &'b BindingPatternKind<'_>,
        set: &mut std::collections::BTreeSet<&'b str>,
    ) {
        match kind {
            BindingPatternKind::BindingIdentifier(b) => {
                set.insert(b.name.as_str());
            }
            BindingPatternKind::ObjectPattern(o) => {
                for p in &o.properties {
                    visit_pattern(&p.value.kind, set);
                }
                if let Some(rest) = &o.rest {
                    visit_pattern(&rest.argument.kind, set);
                }
            }
            BindingPatternKind::ArrayPattern(a) => {
                for e in (&a.elements).into_iter().flatten() {
                    visit_pattern(&e.kind, set);
                }
                if let Some(rest) = &a.rest {
                    visit_pattern(&rest.argument.kind, set);
                }
            }
            BindingPatternKind::AssignmentPattern(a) => visit_pattern(&a.left.kind, set),
        }
    }
    fn visit_stmt<'b>(stmt: &'b Statement<'_>, set: &mut std::collections::BTreeSet<&'b str>) {
        match stmt {
            Statement::VariableDeclaration(v) => {
                for decl in &v.declarations {
                    visit_pattern(&decl.id.kind, set);
                }
            }
            Statement::FunctionDeclaration(f) => {
                if let Some(id) = &f.id {
                    set.insert(id.name.as_str());
                }
            }
            Statement::ClassDeclaration(c) => {
                if let Some(id) = &c.id {
                    set.insert(id.name.as_str());
                }
            }
            Statement::BlockStatement(b) => {
                for s in &b.body {
                    visit_stmt(s, set);
                }
            }
            Statement::IfStatement(i) => {
                visit_stmt(&i.consequent, set);
                if let Some(alt) = &i.alternate {
                    visit_stmt(alt, set);
                }
            }
            Statement::ForStatement(f) => {
                use oxc_ast::ast::ForStatementInit;
                if let Some(ForStatementInit::VariableDeclaration(v)) = &f.init {
                    for decl in &v.declarations {
                        visit_pattern(&decl.id.kind, set);
                    }
                }
                visit_stmt(&f.body, set);
            }
            Statement::ForInStatement(f) => {
                use oxc_ast::ast::ForStatementLeft;
                if let ForStatementLeft::VariableDeclaration(v) = &f.left {
                    for decl in &v.declarations {
                        visit_pattern(&decl.id.kind, set);
                    }
                }
                visit_stmt(&f.body, set);
            }
            Statement::ForOfStatement(f) => {
                use oxc_ast::ast::ForStatementLeft;
                if let ForStatementLeft::VariableDeclaration(v) = &f.left {
                    for decl in &v.declarations {
                        visit_pattern(&decl.id.kind, set);
                    }
                }
                visit_stmt(&f.body, set);
            }
            Statement::WhileStatement(w) => visit_stmt(&w.body, set),
            Statement::DoWhileStatement(d) => visit_stmt(&d.body, set),
            Statement::TryStatement(t) => {
                for s in &t.block.body {
                    visit_stmt(s, set);
                }
                if let Some(handler) = &t.handler {
                    if let Some(param) = &handler.param {
                        visit_pattern(&param.pattern.kind, set);
                    }
                    for s in &handler.body.body {
                        visit_stmt(s, set);
                    }
                }
                if let Some(fin) = &t.finalizer {
                    for s in &fin.body {
                        visit_stmt(s, set);
                    }
                }
            }
            Statement::SwitchStatement(s) => {
                for case in &s.cases {
                    for stmt in &case.consequent {
                        visit_stmt(stmt, set);
                    }
                }
            }
            Statement::LabeledStatement(l) => visit_stmt(&l.body, set),
            _ => {}
        }
    }
    for p in &params.items {
        visit_pattern(&p.pattern.kind, set);
    }
    for stmt in &body.statements {
        visit_stmt(stmt, set);
    }
}

/// Collect **every** identifier name bound anywhere in the program —
/// at module scope AND inside every nested function/method/arrow/class
/// body. Includes formal parameters and local bindings of every inner
/// function expression, declaration, arrow, class, catch handler, and
/// loop binding.
///
/// The motivation is closure-scope minifier resilience. A function
/// fingerprint's `callee_set` filters identifier-callees against the
/// "known local" set. If function `f` calls `helper` where `helper` is
/// bound in an *enclosing* function (not `f`'s own scope, and not the
/// module's top-level), then under the per-function filter `helper`
/// survives — but the minifier renames `helper` to a short alias, so
/// the un-minified fingerprint records `c:helper` while the bundle
/// records `c:K`. The hashes diverge.
///
/// Universal collection closes that gap: every binding name introduced
/// anywhere in the file is in the filter set, so identifier-callees
/// that name an in-file binding drop out of `callee_set` regardless of
/// which function they happen to be referenced from. Built-in globals
/// (`fetch`, `Promise`, `console`, ...) are never bound in normal
/// source and remain visible.
fn collect_universal_renamable_bindings<'p, 'a: 'p>(
    program: &'p oxc_ast::ast::Program<'a>,
) -> std::collections::BTreeSet<&'p str> {
    let mut set = std::collections::BTreeSet::new();
    for stmt in &program.body {
        walk_stmt_bindings(stmt, &mut set);
    }
    set
}

fn walk_pattern_bindings<'p, 'a: 'p>(
    kind: &'p oxc_ast::ast::BindingPatternKind<'a>,
    set: &mut std::collections::BTreeSet<&'p str>,
) {
    use oxc_ast::ast::BindingPatternKind;
    match kind {
        BindingPatternKind::BindingIdentifier(b) => {
            set.insert(b.name.as_str());
        }
        BindingPatternKind::ObjectPattern(o) => {
            for p in &o.properties {
                walk_pattern_bindings(&p.value.kind, set);
                walk_expr_bindings_in_object_property(p, set);
            }
            if let Some(rest) = &o.rest {
                walk_pattern_bindings(&rest.argument.kind, set);
            }
        }
        BindingPatternKind::ArrayPattern(a) => {
            for e in (&a.elements).into_iter().flatten() {
                walk_pattern_bindings(&e.kind, set);
            }
            if let Some(rest) = &a.rest {
                walk_pattern_bindings(&rest.argument.kind, set);
            }
        }
        BindingPatternKind::AssignmentPattern(a) => {
            walk_pattern_bindings(&a.left.kind, set);
            walk_expr_bindings(&a.right, set);
        }
    }
}

fn walk_expr_bindings_in_object_property<'p, 'a: 'p>(
    p: &'p oxc_ast::ast::BindingProperty<'a>,
    set: &mut std::collections::BTreeSet<&'p str>,
) {
    use oxc_ast::ast::PropertyKey;
    if let PropertyKey::StaticIdentifier(_) = &p.key {
        // No binding inside the key; skip.
    } else if let PropertyKey::PrivateIdentifier(_) = &p.key {
        // No binding.
    } else if let Some(expr) = p.key.as_expression() {
        walk_expr_bindings(expr, set);
    }
}

fn walk_stmt_bindings<'p, 'a: 'p>(
    stmt: &'p oxc_ast::ast::Statement<'a>,
    set: &mut std::collections::BTreeSet<&'p str>,
) {
    use oxc_ast::ast::{Declaration, ForStatementInit, ForStatementLeft, Statement};
    match stmt {
        Statement::VariableDeclaration(v) => {
            for decl in &v.declarations {
                walk_pattern_bindings(&decl.id.kind, set);
                if let Some(init) = &decl.init {
                    walk_expr_bindings(init, set);
                }
            }
        }
        Statement::FunctionDeclaration(f) => walk_function_bindings(f, set),
        Statement::ClassDeclaration(c) => walk_class_bindings(c, set),
        Statement::BlockStatement(b) => {
            for s in &b.body {
                walk_stmt_bindings(s, set);
            }
        }
        Statement::IfStatement(i) => {
            walk_expr_bindings(&i.test, set);
            walk_stmt_bindings(&i.consequent, set);
            if let Some(alt) = &i.alternate {
                walk_stmt_bindings(alt, set);
            }
        }
        Statement::ForStatement(f) => {
            if let Some(init) = &f.init {
                match init {
                    ForStatementInit::VariableDeclaration(v) => {
                        for decl in &v.declarations {
                            walk_pattern_bindings(&decl.id.kind, set);
                            if let Some(init) = &decl.init {
                                walk_expr_bindings(init, set);
                            }
                        }
                    }
                    _ => {
                        if let Some(expr) = init.as_expression() {
                            walk_expr_bindings(expr, set);
                        }
                    }
                }
            }
            if let Some(test) = &f.test {
                walk_expr_bindings(test, set);
            }
            if let Some(update) = &f.update {
                walk_expr_bindings(update, set);
            }
            walk_stmt_bindings(&f.body, set);
        }
        Statement::ForInStatement(f) => {
            if let ForStatementLeft::VariableDeclaration(v) = &f.left {
                for decl in &v.declarations {
                    walk_pattern_bindings(&decl.id.kind, set);
                }
            }
            walk_expr_bindings(&f.right, set);
            walk_stmt_bindings(&f.body, set);
        }
        Statement::ForOfStatement(f) => {
            if let ForStatementLeft::VariableDeclaration(v) = &f.left {
                for decl in &v.declarations {
                    walk_pattern_bindings(&decl.id.kind, set);
                }
            }
            walk_expr_bindings(&f.right, set);
            walk_stmt_bindings(&f.body, set);
        }
        Statement::WhileStatement(w) => {
            walk_expr_bindings(&w.test, set);
            walk_stmt_bindings(&w.body, set);
        }
        Statement::DoWhileStatement(d) => {
            walk_expr_bindings(&d.test, set);
            walk_stmt_bindings(&d.body, set);
        }
        Statement::TryStatement(t) => {
            for s in &t.block.body {
                walk_stmt_bindings(s, set);
            }
            if let Some(handler) = &t.handler {
                if let Some(param) = &handler.param {
                    walk_pattern_bindings(&param.pattern.kind, set);
                }
                for s in &handler.body.body {
                    walk_stmt_bindings(s, set);
                }
            }
            if let Some(fin) = &t.finalizer {
                for s in &fin.body {
                    walk_stmt_bindings(s, set);
                }
            }
        }
        Statement::SwitchStatement(s) => {
            walk_expr_bindings(&s.discriminant, set);
            for case in &s.cases {
                if let Some(test) = &case.test {
                    walk_expr_bindings(test, set);
                }
                for stmt in &case.consequent {
                    walk_stmt_bindings(stmt, set);
                }
            }
        }
        Statement::LabeledStatement(l) => walk_stmt_bindings(&l.body, set),
        Statement::ExpressionStatement(e) => walk_expr_bindings(&e.expression, set),
        Statement::ReturnStatement(r) => {
            if let Some(arg) = &r.argument {
                walk_expr_bindings(arg, set);
            }
        }
        Statement::ThrowStatement(t) => walk_expr_bindings(&t.argument, set),
        Statement::ExportNamedDeclaration(e) => {
            if let Some(decl) = &e.declaration {
                match decl {
                    Declaration::VariableDeclaration(v) => {
                        for d in &v.declarations {
                            walk_pattern_bindings(&d.id.kind, set);
                            if let Some(init) = &d.init {
                                walk_expr_bindings(init, set);
                            }
                        }
                    }
                    Declaration::FunctionDeclaration(f) => walk_function_bindings(f, set),
                    Declaration::ClassDeclaration(c) => walk_class_bindings(c, set),
                    _ => {}
                }
            }
        }
        Statement::ExportDefaultDeclaration(d) => {
            use oxc_ast::ast::ExportDefaultDeclarationKind;
            match &d.declaration {
                ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                    walk_function_bindings(f, set);
                }
                ExportDefaultDeclarationKind::ClassDeclaration(c) => {
                    walk_class_bindings(c, set);
                }
                _ => {
                    if let Some(expr) = d.declaration.as_expression() {
                        walk_expr_bindings(expr, set);
                    }
                }
            }
        }
        Statement::ImportDeclaration(i) => {
            // Imports introduce renameable bindings the same way `let`s
            // and `function`s do. Bundlers re-write the bound name when
            // they inline the dependency, so any callee inside this
            // module that names an imported symbol diverges between
            // bundle and source. Collecting them here closes that gap.
            use oxc_ast::ast::ImportDeclarationSpecifier;
            if let Some(specs) = &i.specifiers {
                for spec in specs {
                    match spec {
                        ImportDeclarationSpecifier::ImportSpecifier(s) => {
                            set.insert(s.local.name.as_str());
                        }
                        ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                            set.insert(s.local.name.as_str());
                        }
                        ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                            set.insert(s.local.name.as_str());
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

fn walk_function_bindings<'p, 'a: 'p>(
    f: &'p oxc_ast::ast::Function<'a>,
    set: &mut std::collections::BTreeSet<&'p str>,
) {
    if let Some(id) = &f.id {
        set.insert(id.name.as_str());
    }
    for p in &f.params.items {
        walk_pattern_bindings(&p.pattern.kind, set);
    }
    if let Some(body) = &f.body {
        for stmt in &body.statements {
            walk_stmt_bindings(stmt, set);
        }
    }
}

fn walk_class_bindings<'p, 'a: 'p>(
    c: &'p oxc_ast::ast::Class<'a>,
    set: &mut std::collections::BTreeSet<&'p str>,
) {
    if let Some(id) = &c.id {
        set.insert(id.name.as_str());
    }
    use oxc_ast::ast::ClassElement;
    for elem in &c.body.body {
        match elem {
            ClassElement::MethodDefinition(m) => {
                walk_function_bindings(&m.value, set);
            }
            ClassElement::PropertyDefinition(p) => {
                if let Some(val) = &p.value {
                    walk_expr_bindings(val, set);
                }
            }
            ClassElement::StaticBlock(s) => {
                for stmt in &s.body {
                    walk_stmt_bindings(stmt, set);
                }
            }
            _ => {}
        }
    }
}

fn walk_expr_bindings<'p, 'a: 'p>(
    expr: &'p oxc_ast::ast::Expression<'a>,
    set: &mut std::collections::BTreeSet<&'p str>,
) {
    use oxc_ast::ast::Expression as E;
    match expr {
        E::FunctionExpression(f) => walk_function_bindings(f, set),
        E::ArrowFunctionExpression(a) => {
            for p in &a.params.items {
                walk_pattern_bindings(&p.pattern.kind, set);
            }
            for stmt in &a.body.statements {
                walk_stmt_bindings(stmt, set);
            }
        }
        E::ClassExpression(c) => walk_class_bindings(c, set),
        E::AssignmentExpression(a) => walk_expr_bindings(&a.right, set),
        E::ArrayExpression(arr) => {
            for el in &arr.elements {
                if let Some(expr) = el.as_expression() {
                    walk_expr_bindings(expr, set);
                }
            }
        }
        E::ObjectExpression(o) => {
            use oxc_ast::ast::ObjectPropertyKind;
            for prop in &o.properties {
                if let ObjectPropertyKind::ObjectProperty(p) = prop {
                    walk_expr_bindings(&p.value, set);
                }
            }
        }
        E::CallExpression(c) => {
            walk_expr_bindings(&c.callee, set);
            for arg in &c.arguments {
                if let Some(expr) = arg.as_expression() {
                    walk_expr_bindings(expr, set);
                }
            }
        }
        E::NewExpression(n) => {
            walk_expr_bindings(&n.callee, set);
            for arg in &n.arguments {
                if let Some(expr) = arg.as_expression() {
                    walk_expr_bindings(expr, set);
                }
            }
        }
        E::BinaryExpression(b) => {
            walk_expr_bindings(&b.left, set);
            walk_expr_bindings(&b.right, set);
        }
        E::LogicalExpression(l) => {
            walk_expr_bindings(&l.left, set);
            walk_expr_bindings(&l.right, set);
        }
        E::UnaryExpression(u) => walk_expr_bindings(&u.argument, set),
        E::ConditionalExpression(c) => {
            walk_expr_bindings(&c.test, set);
            walk_expr_bindings(&c.consequent, set);
            walk_expr_bindings(&c.alternate, set);
        }
        E::SequenceExpression(s) => {
            for e in &s.expressions {
                walk_expr_bindings(e, set);
            }
        }
        E::ParenthesizedExpression(p) => walk_expr_bindings(&p.expression, set),
        E::AwaitExpression(a) => walk_expr_bindings(&a.argument, set),
        E::YieldExpression(y) => {
            if let Some(arg) = &y.argument {
                walk_expr_bindings(arg, set);
            }
        }
        E::TemplateLiteral(t) => {
            for e in &t.expressions {
                walk_expr_bindings(e, set);
            }
        }
        E::TaggedTemplateExpression(t) => {
            walk_expr_bindings(&t.tag, set);
            for e in &t.quasi.expressions {
                walk_expr_bindings(e, set);
            }
        }
        E::StaticMemberExpression(m) => walk_expr_bindings(&m.object, set),
        E::ComputedMemberExpression(m) => {
            walk_expr_bindings(&m.object, set);
            walk_expr_bindings(&m.expression, set);
        }
        E::ChainExpression(c) => {
            use oxc_ast::ast::ChainElement;
            match &c.expression {
                ChainElement::CallExpression(call) => {
                    walk_expr_bindings(&call.callee, set);
                    for arg in &call.arguments {
                        if let Some(expr) = arg.as_expression() {
                            walk_expr_bindings(expr, set);
                        }
                    }
                }
                ChainElement::ComputedMemberExpression(m) => {
                    walk_expr_bindings(&m.object, set);
                    walk_expr_bindings(&m.expression, set);
                }
                ChainElement::StaticMemberExpression(m) => walk_expr_bindings(&m.object, set),
                _ => {}
            }
        }
        _ => {}
    }
}

fn locate_function<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
    span: ByteRange,
) -> Option<(&'a FormalParameters<'a>, &'a FunctionBody<'a>)> {
    struct Locator<'a> {
        target: ByteRange,
        found: Option<(&'a FormalParameters<'a>, &'a FunctionBody<'a>)>,
    }
    impl<'a> Visit<'a> for Locator<'a> {
        fn visit_function(&mut self, f: &Function<'a>, flags: ScopeFlags) {
            if self.found.is_some() {
                return;
            }
            let s = f.span();
            if s.start == self.target.start
                && s.end == self.target.end
                && let Some(body) = f.body.as_deref()
            {
                // The visit signature gives us &'_ Function<'a>; we
                // need &'a references. `self.alloc` provided by the
                // Visit trait performs the lifetime extension (it is
                // the same trick oxc's generated walkers use).
                let params: &'a FormalParameters<'a> = self.alloc(&*f.params);
                let body: &'a FunctionBody<'a> = self.alloc(body);
                self.found = Some((params, body));
                return;
            }
            oxc_ast::visit::walk::walk_function(self, f, flags);
        }
        fn visit_arrow_function_expression(&mut self, a: &ArrowFunctionExpression<'a>) {
            if self.found.is_some() {
                return;
            }
            let s = a.span();
            if s.start == self.target.start && s.end == self.target.end {
                let params: &'a FormalParameters<'a> = self.alloc(&*a.params);
                let body: &'a FunctionBody<'a> = self.alloc(&*a.body);
                self.found = Some((params, body));
                return;
            }
            oxc_ast::visit::walk::walk_arrow_function_expression(self, a);
        }
    }
    let mut loc = Locator {
        target: span,
        found: None,
    };
    loc.visit_program(program);
    loc.found
}

#[cfg(test)]
mod fingerprint_tests {
    use super::*;

    #[test]
    fn alpha_renamed_functions_share_primary_ast_hash() {
        let src1 = "function f(a, b) { return a + b; }";
        let src2 = "function g(x, y) { return x + y; }";
        let fp1 = FunctionExtractor::fingerprint(ModuleId(1), src1);
        let fp2 = FunctionExtractor::fingerprint(ModuleId(2), src2);
        assert_eq!(fp1.len(), 1);
        assert_eq!(fp2.len(), 1);
        assert_eq!(fp1[0].primary.ast, fp2[0].primary.ast);
    }

    #[test]
    fn export_keyword_collapse_lands_as_alternate() {
        let src1 = "export function f(a) { return a; }";
        let src2 = "function f(a) { return a; }";
        let fp1 = FunctionExtractor::fingerprint(ModuleId(1), src1);
        let fp2 = FunctionExtractor::fingerprint(ModuleId(2), src2);
        assert!(!fp1.is_empty() && !fp2.is_empty());

        let target = fp2[0].primary.ast;
        let primary_match = fp1[0].primary.ast == target;
        let alt_match = fp1[0].alternates.iter().any(|a| a.axes.ast == target);
        assert!(
            primary_match || alt_match,
            "expected export-normalized variant to align with plain via primary or alternate"
        );
    }

    #[test]
    fn fingerprint_returns_empty_on_parse_error() {
        let fp = FunctionExtractor::fingerprint(ModuleId(1), "function f( { )");
        assert!(fp.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;
    use reverts_ir::ModuleId;

    fn extract(source: &str) -> Vec<ExtractedFunction> {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, source, SourceType::default()).parse();
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        FunctionExtractor::new(ModuleId(1)).extract(&parsed.program)
    }

    #[test]
    fn extractor_records_top_level_functions_with_param_and_stmt_counts() {
        let funcs = extract("function add(a, b) { let s = a + b; return s; }");
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].param_count, 2);
        assert_eq!(funcs[0].statement_count, 2);
        assert!(!funcs[0].is_async);
        assert!(!funcs[0].is_generator);
    }

    #[test]
    fn extractor_walks_into_nested_arrow_and_function() {
        let funcs = extract("function outer() { return (x) => x + 1; }");
        assert_eq!(funcs.len(), 2);
    }

    #[test]
    fn extractor_records_async_and_generator_flags() {
        let funcs = extract("async function a() {}\nfunction* g() { yield 1; }");
        assert_eq!(funcs.len(), 2);
        assert!(funcs.iter().any(|f| f.is_async && !f.is_generator));
        assert!(funcs.iter().any(|f| !f.is_async && f.is_generator));
    }

    #[test]
    fn function_names_recovers_declaration_binding_method_and_assignment_names() {
        let source = "\
function decl(a) { return a; }
const arrow = (b) => b + 1;
const expr = function inner() { return 0; };
const obj = { method(c) { return c; }, prop: (d) => d };
class K { classMethod(e) { return e; } field = (f) => f; }
Target.assigned = function (g) { return g; };
ns.prototype.protoMethod = (h) => h;
[1, 2].map((x) => x * 2);
";
        let extracted = function_names(source);
        let names: std::collections::BTreeSet<&str> =
            extracted.values().map(String::as_str).collect();
        for expected in [
            "decl",
            "arrow",
            "expr", // outer binding wins over the inner function-expression id
            "method",
            "prop",
            "classMethod",
            "field",
            "assigned",
            "protoMethod",
        ] {
            assert!(names.contains(expected), "missing {expected}: {names:?}");
        }
        // The anonymous `.map` callback contributes no name.
        assert!(
            !names.contains("map"),
            "anonymous callback must not be named: {names:?}"
        );
    }

    #[test]
    fn function_names_spans_align_with_fingerprint_function_ids() {
        // The whole approach depends on function_names() and fingerprint()
        // producing identical spans for the same (stripped) source.
        for source in [
            "function classifyForCollapse(a) { return a + 1; }",
            "const classifyForCollapse = (a) => { return a + 1; };",
            "{ const classifyForCollapse = (a) => { return a + 1; }; }",
        ] {
            let fingerprints = FunctionExtractor::fingerprint(ModuleId(7), source);
            assert_eq!(fingerprints.len(), 1, "one fn in: {source}");
            let names = function_names(source);
            assert_eq!(
                names.get(&fingerprints[0].id.span).map(String::as_str),
                Some("classifyForCollapse"),
                "span mismatch for: {source}"
            );
        }
    }
}
