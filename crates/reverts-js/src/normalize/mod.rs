use oxc_allocator::Allocator;
use oxc_ast::ast::Program;
use reverts_ir::NormalizationPassId;

pub trait NormalizationPass {
    fn id(&self) -> NormalizationPassId;
    fn version(&self) -> u32;
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>);
}

pub mod arrow_body_blocked;
pub mod boolean_call_to_double_not_guarded;
pub mod boolean_undefined_canonicalised;
pub mod bundler_wrapper_unwrapped;
pub mod closure_boundary_aligned;
pub mod compound_assignment_canonical;
pub mod computed_to_static_member;
pub mod conditional_boolean_coerced;
pub mod conditional_negation_flipped;
pub mod conditional_statement_expanded;
pub mod constant_string_concat_folded;
pub mod declarator_split;
pub mod empty_else_block_removed;
pub mod equality_negation_flattened;
pub mod export_boundary_normalized;
pub mod for_to_while_finite;
pub mod helper_identity_inlined;
pub mod if_return_else_flattened;
pub mod infinite_for_to_while;
pub mod jsx_runtime_normalized;
pub mod logical_not_chain_flattened;
pub mod logical_short_circuit_expanded;
pub mod nullish_assignment_compacted;
pub mod nullish_equality_compacted;
pub mod number_call_to_unary_plus_guarded;
pub mod return_conditional_expanded;
pub mod sequence_expression_split;
pub mod shadow_check;
pub mod template_no_substitution_lowered;
pub mod trailing_return_void_removed;
pub mod ts_runtime_erased;
pub mod typeof_local_undefined_guarded;
pub mod void_zero_to_undefined_guarded;

#[must_use]
pub fn stable_passes() -> [Box<dyn NormalizationPass + Send + Sync>; 32] {
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
        // `conditional_negation_flipped` MUST run before
        // `return_conditional_expanded` / `conditional_statement_expanded`
        // so the ternary still exists when we strip the leading `!`.
        // Once those expanders convert `return c ? a : b` into an
        // `if/else`, the conditional is gone and the negation cannot
        // be flipped without rewriting if-statements directly.
        Box::new(conditional_negation_flipped::ConditionalNegationFlipped),
        Box::new(conditional_statement_expanded::ConditionalStatementExpanded),
        Box::new(return_conditional_expanded::ReturnConditionalExpanded),
        Box::new(computed_to_static_member::ComputedToStaticMember),
        Box::new(arrow_body_blocked::ArrowBodyBlocked),
        Box::new(compound_assignment_canonical::CompoundAssignmentCanonical),
        Box::new(if_return_else_flattened::IfReturnElseFlattened),
        Box::new(equality_negation_flattened::EqualityNegationFlattened),
        Box::new(constant_string_concat_folded::ConstantStringConcatFolded),
        Box::new(logical_not_chain_flattened::LogicalNotChainFlattened),
        Box::new(template_no_substitution_lowered::TemplateNoSubstitutionLowered),
        Box::new(infinite_for_to_while::InfiniteForToWhile),
        Box::new(for_to_while_finite::ForToWhileFiniteCanonical),
        Box::new(conditional_boolean_coerced::ConditionalBooleanCoerced),
        Box::new(trailing_return_void_removed::TrailingReturnVoidRemoved),
        Box::new(void_zero_to_undefined_guarded::VoidZeroToUndefinedGuarded),
        Box::new(boolean_call_to_double_not_guarded::BooleanCallToDoubleNotGuarded),
        Box::new(number_call_to_unary_plus_guarded::NumberCallToUnaryPlusGuarded),
        Box::new(nullish_equality_compacted::NullishEqualityCompacted),
        Box::new(typeof_local_undefined_guarded::TypeofLocalUndefinedGuarded),
        Box::new(nullish_assignment_compacted::NullishAssignmentCompacted),
        Box::new(empty_else_block_removed::EmptyElseBlockRemoved),
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
        assert_eq!(ids.len(), 32);
    }
}
