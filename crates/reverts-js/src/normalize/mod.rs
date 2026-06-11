use oxc_allocator::Allocator;
use oxc_ast::ast::Program;
use reverts_ir::NormalizationPassId;

pub trait NormalizationPass {
    fn id(&self) -> NormalizationPassId;
    fn version(&self) -> u32;
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>);
}

pub mod bundler_wrapper_unwrapped;
pub mod closure_boundary_aligned;
pub mod export_boundary_normalized;
pub mod helper_identity_inlined;
pub mod jsx_runtime_normalized;
pub mod ts_runtime_erased;

#[must_use]
pub fn stable_passes() -> [Box<dyn NormalizationPass + Send + Sync>; 6] {
    [
        Box::new(ts_runtime_erased::TsRuntimeErased),
        Box::new(jsx_runtime_normalized::JsxRuntimeNormalized),
        Box::new(bundler_wrapper_unwrapped::BundlerWrapperUnwrapped),
        Box::new(helper_identity_inlined::HelperIdentityInlined),
        Box::new(export_boundary_normalized::ExportBoundaryNormalized),
        Box::new(closure_boundary_aligned::ClosureBoundaryAligned),
    ]
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
        assert_eq!(ids.len(), 6);
    }
}
