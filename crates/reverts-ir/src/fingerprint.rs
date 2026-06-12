use crate::FunctionId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AxisKind {
    Ast,
    Cfg,
    ReturnPattern,
    EffectPattern,
    LiteralAnchor,
    AccessPattern,
    StructuralAnchor,
    LiteralShape,
    AccessShape,
    CalleeSet,
    BindingPattern,
    ThrowSet,
}

impl AxisKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ast => "ast",
            Self::Cfg => "cfg",
            Self::ReturnPattern => "return_pattern",
            Self::EffectPattern => "effect_pattern",
            Self::LiteralAnchor => "literal_anchor",
            Self::AccessPattern => "access_pattern",
            Self::StructuralAnchor => "structural_anchor",
            Self::LiteralShape => "literal_shape",
            Self::AccessShape => "access_shape",
            Self::CalleeSet => "callee_set",
            Self::BindingPattern => "binding_pattern",
            Self::ThrowSet => "throw_set",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AxisHashes {
    pub ast: u64,
    pub cfg: u64,
    pub return_pattern: u64,
    pub effect_pattern: u64,
    pub literal_anchor: Option<u64>,
    pub access_pattern: Option<u64>,
    pub structural_anchor: u64,
    pub literal_shape: Option<u64>,
    pub access_shape: Option<u64>,
    pub callee_set: Option<u64>,
    pub binding_pattern: u64,
    pub throw_set: Option<u64>,
}

impl AxisHashes {
    #[must_use]
    pub fn get(&self, axis: AxisKind) -> Option<u64> {
        match axis {
            AxisKind::Ast => Some(self.ast),
            AxisKind::Cfg => Some(self.cfg),
            AxisKind::ReturnPattern => Some(self.return_pattern),
            AxisKind::EffectPattern => Some(self.effect_pattern),
            AxisKind::LiteralAnchor => self.literal_anchor,
            AxisKind::AccessPattern => self.access_pattern,
            AxisKind::StructuralAnchor => Some(self.structural_anchor),
            AxisKind::LiteralShape => self.literal_shape,
            AxisKind::AccessShape => self.access_shape,
            AxisKind::CalleeSet => self.callee_set,
            AxisKind::BindingPattern => Some(self.binding_pattern),
            AxisKind::ThrowSet => self.throw_set,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NormalizationPassId {
    Primary,
    TsRuntimeErased,
    JsxRuntimeNormalized,
    BundlerWrapperUnwrapped,
    HelperIdentityInlined,
    ExportBoundaryNormalized,
    ClosureBoundaryAligned,
    BooleanUndefinedCanonicalised,
    ObjectAssignExpanded,
    DeclaratorSplit,
    SequenceExpressionSplit,
}

impl NormalizationPassId {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::TsRuntimeErased => "ts_runtime_erased",
            Self::JsxRuntimeNormalized => "jsx_runtime_normalized",
            Self::BundlerWrapperUnwrapped => "bundler_wrapper_unwrapped",
            Self::HelperIdentityInlined => "helper_identity_inlined",
            Self::ExportBoundaryNormalized => "export_boundary_normalized",
            Self::ClosureBoundaryAligned => "closure_boundary_aligned",
            Self::BooleanUndefinedCanonicalised => "boolean_undefined_canonicalised",
            Self::ObjectAssignExpanded => "object_assign_expanded",
            Self::DeclaratorSplit => "declarator_split",
            Self::SequenceExpressionSplit => "sequence_expression_split",
        }
    }
}

/// One alternate fingerprint for a function — produced by applying a
/// specific normalization pass and re-extracting. Carries its own
/// `statement_count` because passes like `DeclaratorSplit` change the
/// number of top-level statements; without storing the post-pass count
/// alongside the post-pass hashes, the cascade matcher's
/// `(param_count, statement_count, ast_hash)` lookup would still use
/// the *primary* statement_count and miss matches that the pass was
/// designed to unlock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlternateAxisHashes {
    pub pass: NormalizationPassId,
    pub statement_count: u32,
    pub axes: AxisHashes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionFingerprint {
    pub id: FunctionId,
    pub param_count: u32,
    pub statement_count: u32,
    pub primary: AxisHashes,
    pub alternates: Vec<AlternateAxisHashes>,
}

impl FunctionFingerprint {
    #[must_use]
    pub fn axis_hashes(&self, pass: NormalizationPassId) -> Option<&AxisHashes> {
        if pass == NormalizationPassId::Primary {
            return Some(&self.primary);
        }
        self.alternates
            .iter()
            .find_map(|alt| (alt.pass == pass).then_some(&alt.axes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ByteRange, ModuleId};

    fn sample_axes() -> AxisHashes {
        AxisHashes {
            ast: 1,
            cfg: 2,
            return_pattern: 3,
            effect_pattern: 4,
            literal_anchor: Some(5),
            access_pattern: Some(6),
            structural_anchor: 7,
            literal_shape: None,
            access_shape: Some(8),
            callee_set: Some(9),
            binding_pattern: 10,
            throw_set: None,
        }
    }

    #[test]
    fn axis_hashes_get_returns_optional_axes() {
        let axes = sample_axes();
        assert_eq!(axes.get(AxisKind::Ast), Some(1));
        assert_eq!(axes.get(AxisKind::LiteralShape), None);
        assert_eq!(axes.get(AxisKind::ThrowSet), None);
        assert_eq!(axes.get(AxisKind::StructuralAnchor), Some(7));
    }

    #[test]
    fn function_fingerprint_lookup_finds_primary_and_alternates() {
        let id = FunctionId::new(ModuleId(1), ByteRange::new(0, 10));
        let mut alt = sample_axes();
        alt.ast = 99;
        let fp = FunctionFingerprint {
            id,
            param_count: 2,
            statement_count: 3,
            primary: sample_axes(),
            alternates: vec![(NormalizationPassId::TsRuntimeErased, alt)],
        };

        assert_eq!(
            fp.axis_hashes(NormalizationPassId::Primary).map(|a| a.ast),
            Some(1),
        );
        assert_eq!(
            fp.axis_hashes(NormalizationPassId::TsRuntimeErased)
                .map(|a| a.ast),
            Some(99),
        );
        assert!(
            fp.axis_hashes(NormalizationPassId::JsxRuntimeNormalized)
                .is_none()
        );
    }
}
