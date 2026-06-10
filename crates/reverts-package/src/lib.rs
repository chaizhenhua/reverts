use std::collections::BTreeMap;

use reverts_input::{
    PackageAttributionInput, PackageAttributionStatus, PackageEmissionMode, PackageSurfaceInput,
};
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
    pub fn from_attributions(
        attributions: &[PackageAttributionInput],
        package_surfaces: &[PackageSurfaceInput],
    ) -> Self {
        let mut surfaces = BTreeMap::<String, PackageSurface>::new();

        for attribution in attributions {
            if attribution.status != PackageAttributionStatus::Accepted
                || attribution.emission_mode != PackageEmissionMode::ExternalImport
            {
                continue;
            }

            if let Some(specifier) = attribution.export_specifier.as_deref() {
                insert_surface_specifier(
                    &mut surfaces,
                    attribution.package_name.as_str(),
                    specifier,
                );
            }

            if let Some(subpath) = attribution.subpath.as_deref() {
                insert_surface_subpath(&mut surfaces, attribution.package_name.as_str(), subpath);
            }
        }

        for package_surface in package_surfaces {
            if package_surface.status != PackageAttributionStatus::Accepted {
                continue;
            }
            insert_surface_specifier(
                &mut surfaces,
                package_surface.package_name.as_str(),
                package_surface.export_specifier.as_str(),
            );
        }

        let mut index = Self::default();
        for surface in surfaces.into_values() {
            index.insert(surface);
        }
        index
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
            return PackageResolution::Builtin { specifier: name };
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

fn insert_surface_specifier(
    surfaces: &mut BTreeMap<String, PackageSurface>,
    package_name: &str,
    specifier: &str,
) {
    let Some((resolved_package, subpath)) = split_bare_specifier(specifier) else {
        return;
    };
    if resolved_package != package_name {
        return;
    }

    match subpath {
        Some(subpath) => insert_surface_subpath(surfaces, package_name, subpath.as_str()),
        None => {
            let surface = surfaces
                .remove(package_name)
                .unwrap_or_else(|| PackageSurface::new(package_name))
                .with_root_importable();
            surfaces.insert(package_name.to_string(), surface);
        }
    }
}

fn insert_surface_subpath(
    surfaces: &mut BTreeMap<String, PackageSurface>,
    package_name: &str,
    subpath: &str,
) {
    let surface = surfaces
        .remove(package_name)
        .unwrap_or_else(|| PackageSurface::new(package_name))
        .with_subpath(subpath);
    surfaces.insert(package_name.to_string(), surface);
}

#[must_use]
pub fn is_node_builtin(specifier: &str) -> bool {
    normalize_builtin(specifier).is_some()
}

fn normalize_builtin(specifier: &str) -> Option<String> {
    let name = specifier.strip_prefix("node:").unwrap_or(specifier);
    matches!(
        name,
        "assert"
            | "assert/strict"
            | "async_hooks"
            | "buffer"
            | "child_process"
            | "cluster"
            | "console"
            | "constants"
            | "crypto"
            | "dgram"
            | "diagnostics_channel"
            | "dns"
            | "dns/promises"
            | "domain"
            | "events"
            | "fs"
            | "fs/promises"
            | "http"
            | "http2"
            | "https"
            | "inspector"
            | "module"
            | "net"
            | "os"
            | "path"
            | "path/posix"
            | "path/win32"
            | "perf_hooks"
            | "process"
            | "punycode"
            | "querystring"
            | "readline"
            | "readline/promises"
            | "repl"
            | "stream"
            | "stream/consumers"
            | "stream/promises"
            | "stream/web"
            | "string_decoder"
            | "test"
            | "test/reporters"
            | "timers"
            | "timers/promises"
            | "tls"
            | "trace_events"
            | "tty"
            | "url"
            | "util"
            | "util/types"
            | "v8"
            | "vm"
            | "wasi"
            | "worker_threads"
            | "zlib"
    )
    .then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use reverts_input::{
        PackageAttributionInput, PackageAttributionStatus, PackageEmissionMode, PackageSurfaceInput,
    };
    use reverts_ir::{ModuleId, PackageSurface};

    use super::{PackageResolution, PackageSurfaceIndex, is_node_builtin};

    #[test]
    fn from_attributions_builds_index_from_accepted_external_imports() {
        let attributions = [
            PackageAttributionInput::accepted_external(ModuleId(1), "lodash", "4.17.21", "lodash"),
            PackageAttributionInput::accepted_external(
                ModuleId(2),
                "lodash",
                "4.17.21",
                "lodash/map",
            )
            .with_subpath("map"),
        ];

        let index = PackageSurfaceIndex::from_attributions(&attributions, &[]);

        assert!(matches!(
            index.resolve("lodash"),
            PackageResolution::External { .. }
        ));
        assert!(matches!(
            index.resolve("lodash/map"),
            PackageResolution::External { .. }
        ));
        assert!(matches!(
            index.resolve("lodash/_internal"),
            PackageResolution::Rejected { .. }
        ));
    }

    #[test]
    fn from_attributions_ignores_non_accepted_or_non_external_attributions() {
        let mut vendored =
            PackageAttributionInput::accepted_external(ModuleId(1), "lodash", "4.17.21", "lodash");
        vendored.emission_mode = PackageEmissionMode::VendoredAsset;
        let proposed = PackageAttributionInput::proposed(
            ModuleId(2),
            "lodash",
            Some("4.17.21".to_string()),
            PackageEmissionMode::ExternalImport,
        );
        let attributions = [vendored, proposed];

        let index = PackageSurfaceIndex::from_attributions(&attributions, &[]);

        assert!(matches!(
            index.resolve("lodash"),
            PackageResolution::Rejected { reason, .. } if reason == "package surface is unknown"
        ));
    }

    #[test]
    fn from_attributions_includes_accepted_package_surface_inputs() {
        let surfaces = [PackageSurfaceInput {
            package_name: "react".to_string(),
            package_version: Some("18.2.0".to_string()),
            export_specifier: "react/jsx-runtime".to_string(),
            status: PackageAttributionStatus::Accepted,
            evidence: None,
        }];

        let index = PackageSurfaceIndex::from_attributions(&[], &surfaces);

        assert!(matches!(
            index.resolve("react/jsx-runtime"),
            PackageResolution::External { .. }
        ));
    }

    #[test]
    fn builtin_modules_are_classified_without_package_surface() {
        assert!(is_node_builtin("fs"));
        assert!(is_node_builtin("node:path"));
        assert!(is_node_builtin("fs/promises"));
        assert!(is_node_builtin("node:fs/promises"));
        assert!(is_node_builtin("timers/promises"));
        assert!(is_node_builtin("path/win32"));
        assert!(is_node_builtin("async_hooks"));
        assert!(is_node_builtin("http2"));
        assert!(is_node_builtin("tls"));
        assert!(is_node_builtin("net"));
        assert!(!is_node_builtin("ws"));
        assert!(!is_node_builtin("undici"));
    }

    #[test]
    fn local_and_builtin_specifiers_resolve_without_package_surface() {
        let index = PackageSurfaceIndex::default();

        assert_eq!(index.resolve("./local").specifier(), Some("./local"));
        assert_eq!(index.resolve("../shared").specifier(), Some("../shared"));
        assert_eq!(index.resolve("node:path").specifier(), Some("path"));
        assert_eq!(
            index.resolve("node:fs/promises").specifier(),
            Some("fs/promises")
        );
        assert_eq!(
            index.resolve("timers/promises").specifier(),
            Some("timers/promises")
        );
        assert!(matches!(
            index.resolve("/absolute"),
            PackageResolution::Local { .. }
        ));
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

    #[test]
    fn malformed_or_invalid_bare_specifiers_are_rejected() {
        let index = PackageSurfaceIndex::default();

        assert!(matches!(
            index.resolve(""),
            PackageResolution::Rejected { reason, .. } if reason == "specifier is not importable"
        ));
        assert!(matches!(
            index.resolve("@scope"),
            PackageResolution::Rejected { reason, .. } if reason == "specifier is not importable"
        ));
        assert!(matches!(
            index.resolve("UPPER"),
            PackageResolution::Rejected { reason, .. } if reason == "package name is invalid"
        ));
        assert!(matches!(
            index.resolve("missing"),
            PackageResolution::Rejected { reason, .. } if reason == "package surface is unknown"
        ));
    }

    #[test]
    fn package_resolution_acceptance_matches_emittable_specifier() {
        let accepted = PackageResolution::External {
            specifier: "pkg/sub".to_string(),
            package_name: "pkg".to_string(),
        };
        let rejected = PackageResolution::Rejected {
            specifier: "pkg/missing".to_string(),
            reason: "package surface does not accept subpath".to_string(),
        };

        assert!(accepted.is_accepted());
        assert_eq!(accepted.specifier(), Some("pkg/sub"));
        assert!(!rejected.is_accepted());
        assert_eq!(rejected.specifier(), None);
    }
}
