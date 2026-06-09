use std::collections::BTreeMap;

use reverts_ir::{PackageSurface, is_valid_package_name, split_bare_specifier};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportDecision {
    External(String),
    Rejected(String),
}

#[derive(Debug, Default, Clone)]
pub struct PackageImportResolver {
    surfaces: BTreeMap<String, PackageSurface>,
}

impl PackageImportResolver {
    #[must_use]
    pub fn new(surfaces: impl IntoIterator<Item = PackageSurface>) -> Self {
        Self {
            surfaces: surfaces
                .into_iter()
                .map(|surface| (surface.package_name.clone(), surface))
                .collect(),
        }
    }

    #[must_use]
    pub fn resolve(&self, specifier: &str) -> ImportDecision {
        let Some((package_name, _subpath)) = split_bare_specifier(specifier) else {
            return ImportDecision::Rejected("specifier is not a bare package import".to_string());
        };

        if !is_valid_package_name(&package_name) {
            return ImportDecision::Rejected(format!("invalid package name '{package_name}'"));
        }

        if self
            .surfaces
            .get(&package_name)
            .is_some_and(|surface| surface.accepts(specifier))
        {
            return ImportDecision::External(specifier.to_string());
        }

        ImportDecision::Rejected(format!("package surface does not accept '{specifier}'"))
    }
}

#[cfg(test)]
mod tests {
    use reverts_ir::PackageSurface;

    use super::{ImportDecision, PackageImportResolver};

    #[test]
    fn unresolved_package_subpath_is_not_emitted_as_bare_import() {
        let resolver =
            PackageImportResolver::new([PackageSurface::new("lodash").with_root_importable()]);

        let decision = resolver.resolve("lodash/_mapCacheProto.js");

        assert!(!matches!(
            &decision,
            ImportDecision::External(specifier) if specifier == "lodash/_mapCacheProto.js"
        ));
        assert!(matches!(decision, ImportDecision::Rejected(_)));
    }

    #[test]
    fn valid_declared_subpath_can_remain_external() {
        let resolver = PackageImportResolver::new([PackageSurface::new("@smithy/protocol-http")
            .with_root_importable()
            .with_subpath("dist-es/index.js")]);

        assert_eq!(
            resolver.resolve("@smithy/protocol-http/dist-es/index.js"),
            ImportDecision::External("@smithy/protocol-http/dist-es/index.js".to_string())
        );
    }

    #[test]
    fn invalid_package_name_is_rejected_before_emit() {
        let resolver = PackageImportResolver::default();

        assert!(matches!(
            resolver.resolve("@smithy/XY7"),
            ImportDecision::Rejected(reason) if reason.contains("invalid package name")
        ));
    }
}
