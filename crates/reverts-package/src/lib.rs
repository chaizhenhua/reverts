use std::collections::{BTreeMap, BTreeSet};

use reverts_input::{
    PackageAttributionInput, PackageAttributionStatus, PackageEmissionMode, PackageSurfaceInput,
};
use reverts_ir::{ModuleId, PackageSurface, is_valid_package_name, split_bare_specifier};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PackageSurfaceIndex {
    surfaces: BTreeMap<String, PackageSurface>,
    import_attributes_by_specifier: BTreeMap<String, BTreeMap<String, String>>,
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
        let mut import_attribute_candidates =
            BTreeMap::<String, Option<BTreeMap<String, String>>>::new();
        let mut import_attribute_conflicts = BTreeMap::<String, ()>::new();

        for attribution in attributions {
            if !is_accepted_external_attribution(attribution) {
                continue;
            }

            if let Some(specifier) = attribution.export_specifier.as_deref() {
                insert_surface_specifier(
                    &mut surfaces,
                    attribution.package_name.as_str(),
                    specifier,
                );
                record_import_attribute_candidate(
                    &mut import_attribute_candidates,
                    &mut import_attribute_conflicts,
                    specifier,
                    optional_import_attributes_for_attribution(attribution),
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
        for (specifier, attributes) in import_attribute_candidates {
            if import_attribute_conflicts.contains_key(&specifier) {
                continue;
            }
            let Some(attributes) = attributes.filter(|attributes| !attributes.is_empty()) else {
                continue;
            };
            index
                .import_attributes_by_specifier
                .insert(specifier, attributes);
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
                import_attributes: self
                    .import_attributes_by_specifier
                    .get(specifier)
                    .cloned()
                    .unwrap_or_default(),
            }
        } else {
            PackageResolution::rejected(specifier, "package surface does not accept subpath")
        }
    }
}

#[must_use]
pub fn is_accepted_external_attribution(attribution: &PackageAttributionInput) -> bool {
    attribution.status == PackageAttributionStatus::Accepted
        && attribution.emission_mode == PackageEmissionMode::ExternalImport
}

#[must_use]
pub fn accepted_external_module_ids(
    attributions: &[PackageAttributionInput],
) -> BTreeSet<ModuleId> {
    attributions
        .iter()
        .filter(|attribution| is_accepted_external_attribution(attribution))
        .map(|attribution| attribution.module_id)
        .collect()
}

#[must_use]
pub fn accepted_external_attribution_for_module(
    attributions: &[PackageAttributionInput],
    module_id: ModuleId,
) -> Option<&PackageAttributionInput> {
    attributions.iter().find(|attribution| {
        attribution.module_id == module_id
            && is_accepted_external_attribution(attribution)
            && attribution.export_specifier.is_some()
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageResolution {
    Builtin {
        specifier: String,
    },
    External {
        specifier: String,
        package_name: String,
        import_attributes: BTreeMap<String, String>,
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
            Self::Builtin { specifier } | Self::Local { specifier } => Some(specifier),
            Self::External { specifier, .. } => Some(specifier),
            Self::Rejected { .. } => None,
        }
    }

    #[must_use]
    pub fn import_attributes(&self) -> Option<&BTreeMap<String, String>> {
        match self {
            Self::External {
                import_attributes, ..
            } => Some(import_attributes),
            Self::Builtin { .. } | Self::Local { .. } | Self::Rejected { .. } => None,
        }
    }

    fn rejected(specifier: &str, reason: &str) -> Self {
        Self::Rejected {
            specifier: specifier.to_string(),
            reason: reason.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalImportProofKind {
    DirectSource,
    ExportSpecifierSource,
    RootExportSource,
    NormalizedSourceExport,
    NormalizedSourceAdapter,
    OwnershipSourceMatch,
    SemanticExport,
    SemanticSource,
    SourceMatch,
    DependencyGraphSource,
    DependencyEdgePath,
    CrossVersionSource,
    CrossPackageSource,
    ExportMembers,
    SemanticPath,
    PackageRoot,
    Unknown,
}

impl ExternalImportProofKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::DirectSource => "matched_package_source",
            Self::ExportSpecifierSource => "export_specifier_source",
            Self::RootExportSource => "root_export_source",
            Self::NormalizedSourceExport => "normalized_source_export",
            Self::NormalizedSourceAdapter => "normalized_source_adapter",
            Self::OwnershipSourceMatch => "ownership_source_match",
            Self::SemanticExport => "semantic_export",
            Self::SemanticSource => "semantic_source",
            Self::SourceMatch => "source_match_fallback",
            Self::DependencyGraphSource => "dependency_graph_source",
            Self::DependencyEdgePath => "dependency_edge_path",
            Self::CrossVersionSource => "cross_version_source",
            Self::CrossPackageSource => "cross_package_source",
            Self::ExportMembers => "export_members_adapter",
            Self::SemanticPath => "semantic_path_fallback",
            Self::PackageRoot => "package_root_fallback",
            Self::Unknown => "unknown",
        }
    }
}

#[must_use]
pub fn external_import_proof_kind(source_path: &str) -> ExternalImportProofKind {
    if source_path.starts_with("export-specifier-source:") {
        ExternalImportProofKind::ExportSpecifierSource
    } else if source_path.starts_with("root-export-source:") {
        ExternalImportProofKind::RootExportSource
    } else if source_path.starts_with("normalized-source-export:") {
        ExternalImportProofKind::NormalizedSourceExport
    } else if source_path.starts_with("normalized-source-adapter:") {
        ExternalImportProofKind::NormalizedSourceAdapter
    } else if source_path.starts_with("ownership-source-match:") {
        ExternalImportProofKind::OwnershipSourceMatch
    } else if source_path.starts_with("forced-external:semantic-export:") {
        ExternalImportProofKind::SemanticExport
    } else if source_path.starts_with("forced-external:semantic-source:") {
        ExternalImportProofKind::SemanticSource
    } else if source_path.starts_with("forced-external:source-match:") {
        ExternalImportProofKind::SourceMatch
    } else if source_path.starts_with("forced-external:dependency-graph-source:") {
        ExternalImportProofKind::DependencyGraphSource
    } else if source_path.starts_with("forced-external:dependency-edge-path:") {
        ExternalImportProofKind::DependencyEdgePath
    } else if source_path.starts_with("forced-external:cross-version-source:") {
        ExternalImportProofKind::CrossVersionSource
    } else if source_path.starts_with("forced-external:cross-package-source:") {
        ExternalImportProofKind::CrossPackageSource
    } else if source_path.starts_with("forced-external:export-members:") {
        ExternalImportProofKind::ExportMembers
    } else if source_path.starts_with("forced-external:semantic-path:") {
        ExternalImportProofKind::SemanticPath
    } else if source_path.starts_with("forced-external:package-root:") {
        ExternalImportProofKind::PackageRoot
    } else {
        ExternalImportProofKind::DirectSource
    }
}

#[must_use]
pub fn external_import_proof_label(source_path: &str) -> &'static str {
    external_import_proof_kind(source_path).label()
}

#[must_use]
pub fn external_import_concrete_source_path(proof_path: &str) -> Option<String> {
    if let Some(rest) = proof_path.strip_prefix("normalized-source-export:") {
        return Some(rest.to_string());
    }
    if let Some(rest) = proof_path.strip_prefix("forced-external:source-match:") {
        return Some(rest.to_string());
    }
    if let Some(rest) = proof_path.strip_prefix("forced-external:semantic-source:") {
        return Some(rest.to_string());
    }
    if let Some(rest) = proof_path.strip_prefix("forced-external:semantic-export:") {
        return Some(rest.to_string());
    }
    if let Some(rest) = proof_path.strip_prefix("forced-external:semantic-path:") {
        return Some(rest.to_string());
    }
    if let Some(rest) = proof_path.strip_prefix("forced-external:package-root:") {
        return Some(rest.to_string());
    }
    if let Some(rest) = proof_path.strip_prefix("forced-external:dependency-graph-source:") {
        return rest.rsplit(':').next().map(ToOwned::to_owned);
    }
    if let Some(rest) = proof_path.strip_prefix("forced-external:dependency-edge-path:") {
        return rest.rsplit(':').next().map(ToOwned::to_owned);
    }
    if let Some(rest) = proof_path.strip_prefix("forced-external:cross-version-source:") {
        return rest.rsplit(':').next().map(ToOwned::to_owned);
    }
    if let Some(rest) = proof_path.strip_prefix("forced-external:cross-package-source:") {
        return rest.rsplit(':').next().map(ToOwned::to_owned);
    }
    if let Some(rest) = proof_path.strip_prefix("forced-external:export-members:") {
        return rest.rsplit(':').next().map(ToOwned::to_owned);
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportMembersImportProof {
    pub proof_kind: String,
    pub exported_members: BTreeSet<String>,
    pub aliases: BTreeMap<String, String>,
    pub source_path: String,
}

#[must_use]
pub fn parse_export_members_import_proof(resolved_file: &str) -> Option<ExportMembersImportProof> {
    let rest = resolved_file.strip_prefix("forced-external:export-members:")?;
    let mut parts = rest.splitn(3, ':');
    let proof_kind = parts.next()?.to_string();
    let members = parts.next()?;
    let tail = parts.next().unwrap_or_default();
    let exported_members = members
        .split(',')
        .map(str::trim)
        .filter(|member| is_identifier_like(member))
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>();
    let aliases = export_member_import_proof_aliases(tail);
    let source_path = tail
        .strip_prefix("aliases=")
        .and_then(|rest| {
            rest.split_once(':')
                .map(|(_aliases, source_path)| source_path)
        })
        .unwrap_or(tail)
        .to_string();
    (!proof_kind.is_empty() && !exported_members.is_empty() && !source_path.is_empty()).then_some(
        ExportMembersImportProof {
            proof_kind,
            exported_members,
            aliases,
            source_path,
        },
    )
}

fn export_member_import_proof_aliases(tail: &str) -> BTreeMap<String, String> {
    let Some(alias_tail) = tail.strip_prefix("aliases=") else {
        return BTreeMap::new();
    };
    let aliases = alias_tail
        .split_once(':')
        .map(|(aliases, _source_path)| aliases)
        .unwrap_or(alias_tail);
    aliases
        .split(',')
        .filter_map(|alias| {
            let (local, exported) = alias.split_once('=')?;
            let local = local.trim();
            let exported = exported.trim();
            (is_identifier_like(local) && is_identifier_like(exported))
                .then(|| (local.to_string(), exported.to_string()))
        })
        .collect()
}

fn is_identifier_like(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first == '$' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch == '$' || ch.is_ascii_alphanumeric())
}

fn record_import_attribute_candidate(
    candidates: &mut BTreeMap<String, Option<BTreeMap<String, String>>>,
    conflicts: &mut BTreeMap<String, ()>,
    specifier: &str,
    attributes: Option<BTreeMap<String, String>>,
) {
    use std::collections::btree_map::Entry;

    match candidates.entry(specifier.to_string()) {
        Entry::Vacant(entry) => {
            entry.insert(attributes);
        }
        Entry::Occupied(entry) => {
            if entry.get() != &attributes {
                conflicts.insert(specifier.to_string(), ());
            }
        }
    }
}

#[must_use]
pub fn import_attributes_for_attribution(
    attribution: &PackageAttributionInput,
) -> BTreeMap<String, String> {
    if attribution
        .resolved_file
        .as_deref()
        .is_some_and(is_json_resolved_file)
    {
        return BTreeMap::from([("type".to_string(), "json".to_string())]);
    }
    BTreeMap::new()
}

fn optional_import_attributes_for_attribution(
    attribution: &PackageAttributionInput,
) -> Option<BTreeMap<String, String>> {
    let attributes = import_attributes_for_attribution(attribution);
    (!attributes.is_empty()).then_some(attributes)
}

fn is_json_resolved_file(resolved_file: &str) -> bool {
    resolved_file
        .split(['?', '#'])
        .next()
        .unwrap_or(resolved_file)
        .trim()
        .to_ascii_lowercase()
        .ends_with(".json")
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
    use std::collections::{BTreeMap, BTreeSet};

    use reverts_input::{
        PackageAttributionInput, PackageAttributionStatus, PackageEmissionMode, PackageSurfaceInput,
    };
    use reverts_ir::{ModuleId, PackageSurface};

    use super::{
        ExternalImportProofKind, PackageResolution, PackageSurfaceIndex,
        accepted_external_attribution_for_module, accepted_external_module_ids,
        external_import_concrete_source_path, external_import_proof_kind,
        is_accepted_external_attribution, is_node_builtin, parse_export_members_import_proof,
    };

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
    fn from_attributions_marks_json_package_import_attributes() {
        let attributions = [PackageAttributionInput::accepted_external(
            ModuleId(1),
            "css-color-names",
            "1.0.1",
            "css-color-names",
        )
        .with_resolved_file("css-color-names@1.0.1/css-color-names.json")];

        let index = PackageSurfaceIndex::from_attributions(&attributions, &[]);

        let PackageResolution::External {
            import_attributes, ..
        } = index.resolve("css-color-names")
        else {
            panic!("json package root should resolve");
        };
        assert_eq!(
            import_attributes,
            BTreeMap::from([("type".to_string(), "json".to_string())])
        );
    }

    #[test]
    fn conflicting_import_attribute_evidence_is_not_emitted() {
        let attributions = [
            PackageAttributionInput::accepted_external(ModuleId(1), "pkg", "1.0.0", "pkg")
                .with_resolved_file("pkg@1.0.0/data.json"),
            PackageAttributionInput::accepted_external(ModuleId(2), "pkg", "1.0.0", "pkg")
                .with_resolved_file("pkg@1.0.0/index.js"),
        ];

        let index = PackageSurfaceIndex::from_attributions(&attributions, &[]);

        let PackageResolution::External {
            import_attributes, ..
        } = index.resolve("pkg")
        else {
            panic!("package root should resolve");
        };
        assert!(import_attributes.is_empty());
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
            import_attributes: BTreeMap::new(),
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

    #[test]
    fn accepted_external_helpers_filter_attributions_consistently() {
        let accepted =
            PackageAttributionInput::accepted_external(ModuleId(1), "pkg", "1.0.0", "pkg");
        let rejected = PackageAttributionInput::rejected_source(ModuleId(2), "pkg", "not matched");
        let attributions = vec![accepted.clone(), rejected];

        assert!(is_accepted_external_attribution(&accepted));
        assert_eq!(
            accepted_external_module_ids(&attributions),
            BTreeSet::from([ModuleId(1)])
        );
        assert_eq!(
            accepted_external_attribution_for_module(&attributions, ModuleId(1))
                .map(|attribution| attribution.package_name.as_str()),
            Some("pkg")
        );
        assert!(accepted_external_attribution_for_module(&attributions, ModuleId(2)).is_none());
    }

    #[test]
    fn external_import_proof_parser_classifies_and_extracts_concrete_source_paths() {
        let proof = "forced-external:dependency-graph-source:string-graph:graph=1/1:functions=0:strings=8:pkg@1.0.0/lib/source.js";

        assert_eq!(
            external_import_proof_kind(proof),
            ExternalImportProofKind::DependencyGraphSource
        );
        assert_eq!(
            external_import_concrete_source_path(proof).as_deref(),
            Some("pkg@1.0.0/lib/source.js")
        );
        assert_eq!(
            external_import_proof_kind(
                "forced-external:cross-package-source:source-hash:hint=wrong@1.0.0:graph=0/0:functions=1:strings=2:real@2.0.0/index.js"
            ),
            ExternalImportProofKind::CrossPackageSource
        );
        let cross_version = "forced-external:cross-version-source:normalized_source_hash:from=1.0.0:pkg@2.0.0/lib/runtime.js";
        assert_eq!(
            external_import_proof_kind(cross_version),
            ExternalImportProofKind::CrossVersionSource
        );
        assert_eq!(
            external_import_concrete_source_path(cross_version).as_deref(),
            Some("pkg@2.0.0/lib/runtime.js")
        );
        assert_eq!(
            external_import_concrete_source_path("normalized-source-export:pkg@1.0.0/index.js")
                .as_deref(),
            Some("pkg@1.0.0/index.js")
        );
    }

    #[test]
    fn export_member_proof_parser_preserves_members_aliases_and_source() {
        let proof = parse_export_members_import_proof(
            "forced-external:export-members:commonjs-reexport:PublicError,ErrorCode:aliases=C=PublicError,q=ErrorCode:pkg@1.0.0/index.js",
        )
        .expect("export member proof should parse");

        assert_eq!(proof.proof_kind.as_str(), "commonjs-reexport");
        assert_eq!(
            proof.exported_members,
            BTreeSet::from(["ErrorCode".to_string(), "PublicError".to_string()])
        );
        assert_eq!(
            proof.aliases,
            BTreeMap::from([
                ("C".to_string(), "PublicError".to_string()),
                ("q".to_string(), "ErrorCode".to_string())
            ])
        );
        assert_eq!(proof.source_path.as_str(), "pkg@1.0.0/index.js");
    }
}
