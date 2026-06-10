use std::collections::BTreeMap;

use reverts_ir::{PackageSurface, is_valid_package_name, split_bare_specifier};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PackageSurfaceIndex {
    surfaces: BTreeMap<String, PackageSurface>,
}

impl PackageSurfaceIndex {
    pub fn insert(&mut self, surface: PackageSurface) {
        self.surfaces.insert(surface.package_name.clone(), surface);
    }

    #[must_use]
    pub fn resolve(&self, specifier: &str) -> PackageResolution {
        if specifier.starts_with("./") || specifier.starts_with("../") || specifier.starts_with('/')
        {
            return PackageResolution::Local {
                specifier: specifier.to_string(),
            };
        }

        if let Some(name) = normalize_builtin(specifier) {
            return PackageResolution::Builtin {
                specifier: name.to_string(),
            };
        }

        let Some((package_name, _subpath)) = split_bare_specifier(specifier) else {
            return PackageResolution::rejected(specifier, "specifier is not importable");
        };

        if !is_valid_package_name(&package_name) {
            return PackageResolution::rejected(specifier, "package name is invalid");
        }

        let Some(surface) = self.surfaces.get(&package_name) else {
            return PackageResolution::rejected(specifier, "package surface is unknown");
        };

        if surface.accepts(specifier) {
            PackageResolution::External {
                specifier: specifier.to_string(),
                package_name,
            }
        } else {
            PackageResolution::rejected(specifier, "package surface does not accept subpath")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageResolution {
    Builtin {
        specifier: String,
    },
    External {
        specifier: String,
        package_name: String,
    },
    Local {
        specifier: String,
    },
    Rejected {
        specifier: String,
        reason: String,
    },
}

impl PackageResolution {
    #[must_use]
    pub fn is_accepted(&self) -> bool {
        matches!(
            self,
            Self::Builtin { .. } | Self::External { .. } | Self::Local { .. }
        )
    }

    #[must_use]
    pub fn specifier(&self) -> Option<&str> {
        match self {
            Self::Builtin { specifier }
            | Self::External { specifier, .. }
            | Self::Local { specifier } => Some(specifier),
            Self::Rejected { .. } => None,
        }
    }

    fn rejected(specifier: &str, reason: &str) -> Self {
        Self::Rejected {
            specifier: specifier.to_string(),
            reason: reason.to_string(),
        }
    }
}

#[must_use]
pub fn is_node_builtin(specifier: &str) -> bool {
    normalize_builtin(specifier).is_some()
}

fn normalize_builtin(specifier: &str) -> Option<&str> {
    let name = specifier.strip_prefix("node:").unwrap_or(specifier);
    matches!(
        name,
        "assert"
            | "buffer"
            | "child_process"
            | "crypto"
            | "events"
            | "fs"
            | "http"
            | "https"
            | "module"
            | "os"
            | "path"
            | "process"
            | "stream"
            | "url"
            | "util"
            | "zlib"
    )
    .then_some(name)
}

#[cfg(test)]
mod tests {
    use reverts_ir::PackageSurface;

    use super::{PackageResolution, PackageSurfaceIndex, is_node_builtin};

    #[test]
    fn builtin_modules_are_classified_without_package_surface() {
        assert!(is_node_builtin("fs"));
        assert!(is_node_builtin("node:path"));
    }

    #[test]
    fn absent_package_subpath_is_rejected() {
        let mut index = PackageSurfaceIndex::default();
        index.insert(PackageSurface::new("lodash").with_root_importable());

        assert!(matches!(
            index.resolve("lodash"),
            PackageResolution::External { .. }
        ));
        assert!(matches!(
            index.resolve("lodash/_mapCacheProto.js"),
            PackageResolution::Rejected { .. }
        ));
    }

    #[test]
    fn accepted_resolution_exposes_emittable_specifier() {
        let mut index = PackageSurfaceIndex::default();
        index.insert(PackageSurface::new("lodash").with_root_importable());

        assert_eq!(index.resolve("lodash").specifier(), Some("lodash"));
        assert_eq!(index.resolve("lodash/fp").specifier(), None);
    }
}
