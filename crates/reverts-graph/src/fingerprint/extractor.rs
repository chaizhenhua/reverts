use oxc_allocator::Allocator;
use oxc_ast::Visit;
use oxc_ast::ast::{ArrowFunctionExpression, FormalParameters, Function, FunctionBody, Program};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};
use oxc_syntax::scope::ScopeFlags;
use reverts_ir::{AxisHashes, ByteRange, FunctionFingerprint, FunctionId, ModuleId};
use reverts_js::normalize::{apply_to_source, stable_passes};

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
        let parsed = Parser::new(&alloc, source, source_type).parse();
        if parsed.panicked || !parsed.errors.is_empty() {
            return Vec::new();
        }
        let primary_extracts = Self::new(module_id).extract(&parsed.program);
        let primary_locals = collect_top_level_binding_names(&parsed.program);

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
            let alt_parsed = Parser::new(&alt_alloc, &transformed, source_type).parse();
            if alt_parsed.panicked || !alt_parsed.errors.is_empty() {
                continue;
            }
            let alt_extracts = Self::new(module_id).extract(&alt_parsed.program);
            let alt_locals = collect_top_level_binding_names(&alt_parsed.program);
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

/// Collect every identifier name bound at the program top level (and
/// any nested function/class declaration name that's also program-
/// scope-visible). These names are the rename targets a minifier
/// rewrites — locally introduced and unstable across builds. Function
/// fingerprints filter `callee_set` against this set so that calls to
/// such helpers don't leak unstable names into the hash.
fn collect_top_level_binding_names<'p, 'a: 'p>(
    program: &'p oxc_ast::ast::Program<'a>,
) -> std::collections::BTreeSet<&'p str> {
    use oxc_ast::ast::{BindingPatternKind, Declaration, Statement};
    let mut set = std::collections::BTreeSet::new();
    fn visit_pattern<'p, 'a: 'p>(
        kind: &'p BindingPatternKind<'a>,
        set: &mut std::collections::BTreeSet<&'p str>,
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
    for stmt in &program.body {
        match stmt {
            Statement::VariableDeclaration(v) => {
                for decl in &v.declarations {
                    visit_pattern(&decl.id.kind, &mut set);
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
            Statement::ExportNamedDeclaration(e) => match &e.declaration {
                Some(Declaration::VariableDeclaration(v)) => {
                    for decl in &v.declarations {
                        visit_pattern(&decl.id.kind, &mut set);
                    }
                }
                Some(Declaration::FunctionDeclaration(f)) => {
                    if let Some(id) = &f.id {
                        set.insert(id.name.as_str());
                    }
                }
                Some(Declaration::ClassDeclaration(c)) => {
                    if let Some(id) = &c.id {
                        set.insert(id.name.as_str());
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    set
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
}
