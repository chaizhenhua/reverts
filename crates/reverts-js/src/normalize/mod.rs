use oxc_allocator::Allocator;
use oxc_ast::ast::Program;
use reverts_ir::NormalizationPassId;

pub trait NormalizationPass {
    fn id(&self) -> NormalizationPassId;
    fn version(&self) -> u32;
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>);
}

pub mod arrow_body_blocked;
pub mod boolean_undefined_canonicalised;
pub mod bundler_wrapper_unwrapped;
pub mod closure_boundary_aligned;
pub mod compound_assignment_canonical;
pub mod computed_to_static_member;
pub mod conditional_statement_expanded;
pub mod declarator_split;
pub mod export_boundary_normalized;
pub mod helper_identity_inlined;
pub mod if_return_else_flattened;
pub mod jsx_runtime_normalized;
pub mod logical_short_circuit_expanded;
pub mod return_conditional_expanded;
pub mod sequence_expression_split;
pub mod ts_runtime_erased;

#[must_use]
pub fn stable_passes() -> [Box<dyn NormalizationPass + Send + Sync>; 16] {
    [
        Box::new(ts_runtime_erased::TsRuntimeErased),
        Box::new(jsx_runtime_normalized::JsxRuntimeNormalized),
        Box::new(bundler_wrapper_unwrapped::BundlerWrapperUnwrapped),
        Box::new(helper_identity_inlined::HelperIdentityInlined),
        Box::new(export_boundary_normalized::ExportBoundaryNormalized),
        Box::new(closure_boundary_aligned::ClosureBoundaryAligned),
        Box::new(boolean_undefined_canonicalised::BooleanUndefinedCanonicalised),
        Box::new(declarator_split::DeclaratorSplit),
        Box::new(sequence_expression_split::SequenceExpressionSplit),
        Box::new(logical_short_circuit_expanded::LogicalShortCircuitExpanded),
        Box::new(conditional_statement_expanded::ConditionalStatementExpanded),
        Box::new(return_conditional_expanded::ReturnConditionalExpanded),
        Box::new(computed_to_static_member::ComputedToStaticMember),
        Box::new(arrow_body_blocked::ArrowBodyBlocked),
        Box::new(compound_assignment_canonical::CompoundAssignmentCanonical),
        Box::new(if_return_else_flattened::IfReturnElseFlattened),
    ]
}

use oxc_codegen::{CodeGenerator, CodegenOptions};
use oxc_parser::Parser;
use oxc_span::SourceType;

/// Test/debug helper. Parses TypeScript-permissive source, runs `pass`,
/// re-emits, returns the printed string. Bails if the input fails to parse.
pub fn apply_to_source(pass: &dyn NormalizationPass, source: &str) -> Result<String, String> {
    let alloc = Allocator::default();
    let source_type = SourceType::default().with_typescript(true).with_jsx(true);
    let parsed = Parser::new(&alloc, source, source_type).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return Err(format!(
            "parse failed: {}",
            parsed
                .errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }
    let mut program = parsed.program;
    pass.apply(&alloc, &mut program);
    let printed = CodeGenerator::new()
        .with_options(CodegenOptions::default())
        .build(&program);
    Ok(printed.code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn stable_passes_have_unique_ids_and_non_zero_versions() {
        let passes = stable_passes();
        let mut ids = BTreeSet::new();
        for pass in passes.iter() {
            assert_ne!(
                pass.id(),
                NormalizationPassId::Primary,
                "passes must not use Primary id"
            );
            assert!(pass.version() > 0, "pass version must be non-zero");
            assert!(ids.insert(pass.id()), "duplicate pass id: {:?}", pass.id());
        }
        assert_eq!(ids.len(), 16);
    }
}
