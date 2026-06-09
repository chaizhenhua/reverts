use reverts_ir::BindingShape;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingMaterialization {
    pub binding: String,
    pub shape: BindingShape,
    pub source: String,
}

#[must_use]
pub fn materialize_binding_from_source(
    binding: &str,
    shape: BindingShape,
    source: impl Into<String>,
) -> BindingMaterialization {
    BindingMaterialization {
        binding: binding.to_string(),
        shape,
        source: source.into(),
    }
}

#[cfg(test)]
mod tests {
    use reverts_ir::BindingShape;

    use super::materialize_binding_from_source;

    #[test]
    fn binding_materialization_preserves_real_source() {
        let emitted = materialize_binding_from_source(
            "fetchUser",
            BindingShape::Callable,
            "export function fetchUser() { return 42; }",
        );

        assert_eq!(emitted.binding, "fetchUser");
        assert!(emitted.source.contains("function fetchUser"));
        assert!(!emitted.source.contains("undefined"));
    }
}
