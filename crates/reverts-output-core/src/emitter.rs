use reverts_ir::BindingShape;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingMaterialization {
    pub binding: String,
    pub shape: BindingShape,
    pub source: String,
}

#[must_use]
pub fn materialize_binding(binding: &str, shape: BindingShape) -> BindingMaterialization {
    let initializer = match shape {
        BindingShape::Callable => "(..._args: any[]) => undefined",
        BindingShape::Constructor | BindingShape::ClassLike => "class {}",
        BindingShape::NamespaceObject | BindingShape::PlainObject | BindingShape::EnumObject => {
            "{}"
        }
        BindingShape::Unknown | BindingShape::Value => "undefined as any",
    };
    BindingMaterialization {
        binding: binding.to_string(),
        shape,
        source: format!("const {binding}: any = {initializer};"),
    }
}

#[cfg(test)]
mod tests {
    use reverts_ir::BindingShape;

    use super::materialize_binding;

    #[test]
    fn callable_placeholder_is_not_emitted_as_object() {
        let emitted = materialize_binding("zz", BindingShape::Callable);

        assert!(emitted.source.contains("=>"));
        assert!(!emitted.source.contains("= {};"));
    }

    #[test]
    fn enum_initializer_is_emitted_as_initialized_object_binding() {
        let emitted = materialize_binding("NativeModuleType", BindingShape::EnumObject);

        assert_eq!(
            emitted.source,
            "const NativeModuleType: any = {};".to_string()
        );
    }
}
