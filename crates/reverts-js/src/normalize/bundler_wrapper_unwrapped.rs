use oxc_allocator::Allocator;
use oxc_ast::ast::Program;
use reverts_ir::NormalizationPassId;

use super::NormalizationPass;

pub struct BundlerWrapperUnwrapped;

impl NormalizationPass for BundlerWrapperUnwrapped {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::BundlerWrapperUnwrapped
    }
    fn version(&self) -> u32 {
        1
    }
    fn apply<'a>(&self, _alloc: &'a Allocator, _program: &mut Program<'a>) {
        // Body filled in by Task 8.
    }
}
