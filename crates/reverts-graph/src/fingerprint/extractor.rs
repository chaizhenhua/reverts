use oxc_allocator::Allocator;
use oxc_ast::Visit;
use oxc_ast::ast::{
    ArrowFunctionExpression, Declaration, FormalParameters, Function, FunctionBody, Program,
    Statement,
};
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
    #[must_use]
    pub fn fingerprint(module_id: ModuleId, source: &str) -> Vec<FunctionFingerprint> {
        let alloc = Allocator::default();
        let source_type = SourceType::default().with_typescript(true).with_jsx(true);
        let parsed = Parser::new(&alloc, source, source_type).parse();
        if parsed.panicked || !parsed.errors.is_empty() {
            return Vec::new();
        }
        let primary_extracts = Self::new(module_id).extract(&parsed.program);

        let mut out: Vec<FunctionFingerprint> = primary_extracts
            .iter()
            .filter_map(|f| {
                let (params, body) = locate_function(&parsed.program, f.id.span)?;
                Some(FunctionFingerprint {
                    id: f.id,
                    param_count: f.param_count,
                    statement_count: f.statement_count,
                    primary: compute_axes(params, body),
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
                fp.alternates.push((pass.id(), compute_axes(params, body)));
            }
        }
        out
    }
}

fn compute_axes<'a>(params: &FormalParameters<'a>, body: &FunctionBody<'a>) -> AxisHashes {
    let (acc_p, acc_s) = super::access::compute(body);
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
        callee_set: super::callee_set::compute(body),
        binding_pattern: super::binding_pattern::compute(params, body),
        throw_set: super::throw_set::compute(body),
    }
}

fn locate_function<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
    span: ByteRange,
) -> Option<(&'a FormalParameters<'a>, &'a FunctionBody<'a>)> {
    for stmt in &program.body {
        match stmt {
            Statement::FunctionDeclaration(f) => {
                let s = f.span();
                if s.start == span.start && s.end == span.end {
                    let body = f.body.as_deref()?;
                    return Some((&f.params, body));
                }
            }
            Statement::ExportNamedDeclaration(exp) => {
                if let Some(Declaration::FunctionDeclaration(f)) = &exp.declaration {
                    let s = f.span();
                    if s.start == span.start && s.end == span.end {
                        let body = f.body.as_deref()?;
                        return Some((&f.params, body));
                    }
                }
            }
            Statement::ExportDefaultDeclaration(exp) => {
                use oxc_ast::ast::ExportDefaultDeclarationKind;
                if let ExportDefaultDeclarationKind::FunctionDeclaration(f) = &exp.declaration {
                    let s = f.span();
                    if s.start == span.start && s.end == span.end {
                        let body = f.body.as_deref()?;
                        return Some((&f.params, body));
                    }
                }
            }
            _ => {}
        }
    }
    None
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
        let alt_match = fp1[0].alternates.iter().any(|(_, a)| a.ast == target);
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
