use oxc_ast::Visit;
use oxc_ast::ast::{ArrowFunctionExpression, Function, Program};
use oxc_span::GetSpan;
use oxc_syntax::scope::ScopeFlags;
use reverts_ir::{ByteRange, FunctionId, ModuleId};

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
