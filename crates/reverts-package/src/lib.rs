use std::collections::{BTreeMap, BTreeSet};

use reverts_input::{
    ModuleInput, PackageAttributionInput, PackageAttributionStatus, PackageEmissionMode,
    PackageSurfaceInput,
};
use reverts_ir::{
    ModuleId, ModuleKind, PackageSurface, is_identifier_like_ascii, is_valid_package_name,
    split_bare_specifier,
};

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

/// Parse package metadata carried as plain `package.json` text or as the
/// cache-normalized `export default { ... };` wrapper.
#[must_use]
pub fn parse_package_json_source(source: &str) -> Option<serde_json::Value> {
    let trimmed = source.trim();
    let body = trimmed
        .strip_prefix("export default")
        .map(str::trim_start)
        .unwrap_or(trimmed);
    let body = body.trim().trim_end_matches(';').trim();
    serde_json::from_str::<serde_json::Value>(body).ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageSpecifierPublicProof {
    Public(PackageSpecifierPublicReason),
    NotPublic(PackageSpecifierNotPublicReason),
}

impl PackageSpecifierPublicProof {
    #[must_use]
    pub const fn is_public(&self) -> bool {
        matches!(self, Self::Public(_))
    }

    #[must_use]
    pub const fn not_public_reason(&self) -> Option<&PackageSpecifierNotPublicReason> {
        match self {
            Self::Public(_) => None,
            Self::NotPublic(reason) => Some(reason),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageSpecifierPublicReason {
    ExportsSubpath,
    RootConditionalExports,
    RootStringExports,
    RootMain,
    RootModule,
    RootIndex,
    UnrestrictedDeepImport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageSpecifierNotPublicReason {
    InvalidPackageJson,
    SpecifierOutsidePackage,
    EmptySubpath,
    ExportsSubpathNotExported,
    ExportsSubpathBlocked,
    ExportsSubpathHasNoRuntimeTarget,
    RootOnlyExports,
    RootExportsHasNoRuntimeTarget,
    MissingRootEntry,
}

/// Whether `specifier` is publicly importable from `package_name`, using the
/// package's cached `package.json` source and whether the package ships a root
/// `index.{js,json,node}` default entry. Invalid metadata is conservative:
/// unproven specifiers are treated as non-public.
#[must_use]
pub fn package_specifier_is_public(
    package_json_source: &str,
    package_name: &str,
    specifier: &str,
    has_root_index: bool,
) -> bool {
    package_specifier_public_proof(package_json_source, package_name, specifier, has_root_index)
        .is_public()
}

#[must_use]
pub fn package_specifier_public_proof(
    package_json_source: &str,
    package_name: &str,
    specifier: &str,
    has_root_index: bool,
) -> PackageSpecifierPublicProof {
    match parse_package_json_source(package_json_source) {
        Some(package_json) => package_specifier_public_proof_from_manifest(
            package_name,
            &package_json,
            specifier,
            has_root_index,
        ),
        None => PackageSpecifierPublicProof::NotPublic(
            PackageSpecifierNotPublicReason::InvalidPackageJson,
        ),
    }
}

/// Whether `specifier` is publicly importable from `package_name`, using an
/// already-parsed package manifest.
///
/// This intentionally models the package public-surface policy, not matcher
/// scoring: callers must prove that a matched module corresponds to this
/// specifier separately before they emit an external import.
#[must_use]
pub fn package_specifier_is_public_from_manifest(
    package_name: &str,
    package_json: &serde_json::Value,
    specifier: &str,
    has_root_index: bool,
) -> bool {
    package_specifier_public_proof_from_manifest(
        package_name,
        package_json,
        specifier,
        has_root_index,
    )
    .is_public()
}

#[must_use]
pub fn package_specifier_public_proof_from_manifest(
    package_name: &str,
    package_json: &serde_json::Value,
    specifier: &str,
    has_root_index: bool,
) -> PackageSpecifierPublicProof {
    let subpath = if specifier == package_name {
        ".".to_string()
    } else if let Some(rest) = specifier.strip_prefix(package_name) {
        match rest.strip_prefix('/') {
            Some(sub) if !sub.is_empty() => format!("./{sub}"),
            Some(_) => {
                return PackageSpecifierPublicProof::NotPublic(
                    PackageSpecifierNotPublicReason::EmptySubpath,
                );
            }
            None => {
                return PackageSpecifierPublicProof::NotPublic(
                    PackageSpecifierNotPublicReason::SpecifierOutsidePackage,
                );
            }
        }
    } else {
        return PackageSpecifierPublicProof::NotPublic(
            PackageSpecifierNotPublicReason::SpecifierOutsidePackage,
        );
    };

    match package_json.get("exports") {
        Some(exports @ serde_json::Value::Object(map)) => {
            if map.keys().any(|key| key == "." || key.starts_with("./")) {
                exports_subpath_public_proof(map, &subpath)
            } else if subpath == "." {
                if exports_target_has_runtime_resolution(exports) {
                    PackageSpecifierPublicProof::Public(
                        PackageSpecifierPublicReason::RootConditionalExports,
                    )
                } else {
                    PackageSpecifierPublicProof::NotPublic(
                        PackageSpecifierNotPublicReason::RootExportsHasNoRuntimeTarget,
                    )
                }
            } else {
                // Root-only conditions object (e.g. { import, require }).
                PackageSpecifierPublicProof::NotPublic(
                    PackageSpecifierNotPublicReason::RootOnlyExports,
                )
            }
        }
        Some(serde_json::Value::String(_)) => {
            if subpath == "." {
                PackageSpecifierPublicProof::Public(PackageSpecifierPublicReason::RootStringExports)
            } else {
                PackageSpecifierPublicProof::NotPublic(
                    PackageSpecifierNotPublicReason::RootOnlyExports,
                )
            }
        }
        Some(value) => {
            if subpath == "." && exports_target_has_runtime_resolution(value) {
                PackageSpecifierPublicProof::Public(
                    PackageSpecifierPublicReason::RootConditionalExports,
                )
            } else if subpath == "." {
                PackageSpecifierPublicProof::NotPublic(
                    PackageSpecifierNotPublicReason::RootExportsHasNoRuntimeTarget,
                )
            } else {
                PackageSpecifierPublicProof::NotPublic(
                    PackageSpecifierNotPublicReason::RootOnlyExports,
                )
            }
        }
        _ => {
            if subpath == "." {
                if package_json.get("main").is_some() {
                    PackageSpecifierPublicProof::Public(PackageSpecifierPublicReason::RootMain)
                } else if package_json.get("module").is_some() {
                    PackageSpecifierPublicProof::Public(PackageSpecifierPublicReason::RootModule)
                } else if has_root_index {
                    PackageSpecifierPublicProof::Public(PackageSpecifierPublicReason::RootIndex)
                } else {
                    PackageSpecifierPublicProof::NotPublic(
                        PackageSpecifierNotPublicReason::MissingRootEntry,
                    )
                }
            } else {
                // No `exports` allowlist: existing files are importable by
                // Node resolution. The caller is responsible for proving the
                // file/specifier exists.
                PackageSpecifierPublicProof::Public(
                    PackageSpecifierPublicReason::UnrestrictedDeepImport,
                )
            }
        }
    }
}

fn exports_subpath_public_proof(
    map: &serde_json::Map<String, serde_json::Value>,
    subpath: &str,
) -> PackageSpecifierPublicProof {
    if let Some(target) = map.get(subpath) {
        if target.is_null() {
            return PackageSpecifierPublicProof::NotPublic(
                PackageSpecifierNotPublicReason::ExportsSubpathBlocked,
            );
        }
        return if exports_target_has_runtime_resolution(target) {
            PackageSpecifierPublicProof::Public(PackageSpecifierPublicReason::ExportsSubpath)
        } else {
            PackageSpecifierPublicProof::NotPublic(
                PackageSpecifierNotPublicReason::ExportsSubpathHasNoRuntimeTarget,
            )
        };
    }
    for (key, target) in map {
        if exports_pattern_matches(key, subpath) {
            if target.is_null() {
                return PackageSpecifierPublicProof::NotPublic(
                    PackageSpecifierNotPublicReason::ExportsSubpathBlocked,
                );
            }
            return if exports_target_has_runtime_resolution(target) {
                PackageSpecifierPublicProof::Public(PackageSpecifierPublicReason::ExportsSubpath)
            } else {
                PackageSpecifierPublicProof::NotPublic(
                    PackageSpecifierNotPublicReason::ExportsSubpathHasNoRuntimeTarget,
                )
            };
        }
    }
    PackageSpecifierPublicProof::NotPublic(
        PackageSpecifierNotPublicReason::ExportsSubpathNotExported,
    )
}

fn exports_pattern_matches(pattern: &str, subpath: &str) -> bool {
    let Some((prefix, suffix)) = pattern.split_once('*') else {
        return false;
    };
    subpath.starts_with(prefix)
        && subpath.ends_with(suffix)
        && subpath.len() > prefix.len() + suffix.len()
}

fn exports_target_has_runtime_resolution(target: &serde_json::Value) -> bool {
    match target {
        serde_json::Value::String(_) => true,
        serde_json::Value::Array(items) => items.iter().any(exports_target_has_runtime_resolution),
        serde_json::Value::Object(map) => map.iter().any(|(condition, value)| {
            condition != "types"
                && condition != "typings"
                && exports_target_has_runtime_resolution(value)
        }),
        _ => false,
    }
}

#[must_use]
pub fn resolve_package_deep_import_specifier(
    package_json_source: &str,
    package_name: &str,
    specifier: &str,
    entry_paths: &BTreeSet<String>,
) -> Option<String> {
    if package_has_exports_field(package_json_source) {
        return Some(specifier.to_string());
    }
    let Some(subpath) = specifier.strip_prefix(format!("{package_name}/").as_str()) else {
        return Some(specifier.to_string());
    };
    if entry_paths.contains(subpath) {
        return Some(specifier.to_string());
    }
    const EXTENSIONS: [&str; 5] = [".js", ".json", ".cjs", ".mjs", ".node"];
    for extension in EXTENSIONS {
        let file = format!("{subpath}{extension}");
        if entry_paths.contains(file.as_str()) {
            return Some(format!("{package_name}/{file}"));
        }
    }
    for extension in EXTENSIONS {
        let file = format!("{subpath}/index{extension}");
        if entry_paths.contains(file.as_str()) {
            return Some(format!("{package_name}/{file}"));
        }
    }
    None
}

fn package_has_exports_field(package_json_source: &str) -> bool {
    parse_package_json_source(package_json_source)
        .is_some_and(|manifest| manifest.get("exports").is_some())
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

#[must_use]
pub fn same_package_consumer(module: &ModuleInput, consumer: &ModuleInput) -> bool {
    let Some(module_package) = module.package_name.as_deref().map(str::trim) else {
        return false;
    };
    let Some(consumer_package) = consumer.package_name.as_deref().map(str::trim) else {
        return false;
    };
    !module_package.is_empty() && module_package == consumer_package
}

#[must_use]
pub fn external_import_consumer_is_boundary(module: &ModuleInput, consumer: &ModuleInput) -> bool {
    match consumer.kind {
        ModuleKind::Application => true,
        ModuleKind::Package => !same_package_consumer(module, consumer),
        ModuleKind::Builtin => true,
    }
}

#[must_use]
pub fn source_suppressed_consumer_is_boundary(
    module: &ModuleInput,
    consumer: &ModuleInput,
) -> bool {
    match consumer.kind {
        ModuleKind::Application => true,
        ModuleKind::Package => !same_package_consumer(module, consumer),
        ModuleKind::Builtin => false,
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
            Self::SourceMatch => "source_match",
            Self::DependencyGraphSource => "dependency_graph_source",
            Self::DependencyEdgePath => "dependency_edge_path",
            Self::CrossVersionSource => "cross_version_source",
            Self::CrossPackageSource => "cross_package_source",
            Self::ExportMembers => "export_members_adapter",
            Self::SemanticPath => "semantic_path",
            Self::PackageRoot => "package_root",
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
        .filter(|member| is_identifier_like_ascii(member))
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
            (is_identifier_like_ascii(local) && is_identifier_like_ascii(exported))
                .then(|| (local.to_string(), exported.to_string()))
        })
        .collect()
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
        ExternalImportProofKind, PackageResolution, PackageSpecifierNotPublicReason,
        PackageSpecifierPublicProof, PackageSpecifierPublicReason, PackageSurfaceIndex,
        accepted_external_attribution_for_module, accepted_external_module_ids,
        external_import_concrete_source_path, external_import_proof_kind,
        external_import_proof_label, is_accepted_external_attribution, is_node_builtin,
        package_specifier_is_public, package_specifier_public_proof,
        parse_export_members_import_proof, parse_package_json_source,
        resolve_package_deep_import_specifier,
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
    fn package_public_surface_matches_exports_exact_and_pattern() {
        let package_json = r#"export default {
            "name": "rxjs",
            "exports": {
                ".": "./dist/index.js",
                "./operators": "./dist/operators.js",
                "./fetch/*": "./dist/fetch/*.js",
                "./internal/*": null
            }
        };"#;

        assert!(package_specifier_is_public(
            package_json,
            "rxjs",
            "rxjs",
            false
        ));
        assert!(package_specifier_is_public(
            package_json,
            "rxjs",
            "rxjs/operators",
            false
        ));
        assert!(package_specifier_is_public(
            package_json,
            "rxjs",
            "rxjs/fetch/client",
            false
        ));
        assert!(!package_specifier_is_public(
            package_json,
            "rxjs",
            "rxjs/internal/util",
            false
        ));
    }

    #[test]
    fn package_public_surface_is_conservative_for_invalid_manifest() {
        assert!(!package_specifier_is_public("not json", "pkg", "pkg", true));
        assert_eq!(
            package_specifier_public_proof("not json", "pkg", "pkg", true),
            PackageSpecifierPublicProof::NotPublic(
                PackageSpecifierNotPublicReason::InvalidPackageJson
            )
        );
    }

    #[test]
    fn package_public_surface_matches_root_only_exports() {
        let conditions =
            r#"{"name":"ws","exports":{"import":"./wrapper.mjs","require":"./wrapper.js"}}"#;
        assert!(package_specifier_is_public(conditions, "ws", "ws", false));
        assert!(!package_specifier_is_public(
            conditions, "ws", "ws/lib/x", false
        ));

        let string_exports = r#"{"name":"ws","exports":"./wrapper.js"}"#;
        assert!(package_specifier_is_public(
            string_exports,
            "ws",
            "ws",
            false
        ));
        assert!(!package_specifier_is_public(
            string_exports,
            "ws",
            "ws/lib/x",
            false
        ));
    }

    #[test]
    fn package_public_surface_allows_proven_deep_imports_without_exports() {
        let package_json = r#"{"name":"semver","main":"./index.js"}"#;
        assert!(package_specifier_is_public(
            package_json,
            "semver",
            "semver",
            false
        ));
        assert!(package_specifier_is_public(
            package_json,
            "semver",
            "semver/classes/range.js",
            false
        ));

        let no_entry = r#"{"name":"semver"}"#;
        assert!(!package_specifier_is_public(
            no_entry, "semver", "semver", false
        ));
        assert!(package_specifier_is_public(
            no_entry, "semver", "semver", true
        ));
        assert!(package_specifier_is_public(
            no_entry,
            "semver",
            "semver/classes/range.js",
            false
        ));
    }

    #[test]
    fn package_json_source_parser_accepts_cache_wrapped_and_plain_json() {
        let wrapped =
            r#"export default {"name":"rxjs","version":"7.8.1","main":"./dist/cjs/index.js"};"#;
        assert_eq!(
            parse_package_json_source(wrapped)
                .and_then(|value| value.get("main").cloned())
                .and_then(|value| value.as_str().map(str::to_string)),
            Some("./dist/cjs/index.js".to_string())
        );
        assert_eq!(
            parse_package_json_source(r#"{"name":"x","version":"1.0.0"}"#)
                .and_then(|value| value.get("name").cloned())
                .and_then(|value| value.as_str().map(str::to_string)),
            Some("x".to_string())
        );
    }

    #[test]
    fn package_public_surface_reports_proof_reasons() {
        assert_eq!(
            package_specifier_public_proof(
                r#"{"name":"pkg","exports":{"./feature":["./feature.mjs",null]}}"#,
                "pkg",
                "pkg/feature",
                false,
            ),
            PackageSpecifierPublicProof::Public(PackageSpecifierPublicReason::ExportsSubpath)
        );
        assert_eq!(
            package_specifier_public_proof(
                r#"{"name":"pkg","exports":{"types":"./index.d.ts"}}"#,
                "pkg",
                "pkg",
                false,
            ),
            PackageSpecifierPublicProof::NotPublic(
                PackageSpecifierNotPublicReason::RootExportsHasNoRuntimeTarget
            )
        );
        assert_eq!(
            package_specifier_public_proof(
                r#"{"name":"pkg","exports":{"./private":null}}"#,
                "pkg",
                "pkg/private",
                false,
            ),
            PackageSpecifierPublicProof::NotPublic(
                PackageSpecifierNotPublicReason::ExportsSubpathBlocked
            )
        );
        assert_eq!(
            package_specifier_public_proof(
                r#"{"name":"pkg","exports":{"./types":{"types":"./types.d.ts"}}}"#,
                "pkg",
                "pkg/types",
                false,
            ),
            PackageSpecifierPublicProof::NotPublic(
                PackageSpecifierNotPublicReason::ExportsSubpathHasNoRuntimeTarget
            )
        );
    }

    #[test]
    fn package_deep_import_resolution_uses_cached_files_for_no_exports_packages() {
        let entry_paths = BTreeSet::from([
            "_baseUnary.js".to_string(),
            "collection/index.cjs".to_string(),
            "lodash.js".to_string(),
        ]);

        assert_eq!(
            resolve_package_deep_import_specifier(
                r#"{"name":"lodash","main":"./lodash.js"}"#,
                "lodash",
                "lodash/_baseUnary",
                &entry_paths,
            ),
            Some("lodash/_baseUnary.js".to_string())
        );
        assert_eq!(
            resolve_package_deep_import_specifier(
                r#"{"name":"lodash","main":"./lodash.js"}"#,
                "lodash",
                "lodash/collection",
                &entry_paths,
            ),
            Some("lodash/collection/index.cjs".to_string())
        );
        assert_eq!(
            resolve_package_deep_import_specifier(
                r#"{"name":"lodash","main":"./lodash.js"}"#,
                "lodash",
                "lodash/missing",
                &entry_paths,
            ),
            None
        );
        assert_eq!(
            resolve_package_deep_import_specifier(
                r#"{"name":"pkg","exports":{"./feature":"./feature.js"}}"#,
                "pkg",
                "pkg/feature",
                &BTreeSet::new(),
            ),
            Some("pkg/feature".to_string())
        );
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
        assert_eq!(
            external_import_proof_label("forced-external:source-match:pkg@1.0.0/index.js"),
            "source_match"
        );
        assert_eq!(
            external_import_proof_label("forced-external:semantic-path:pkg@1.0.0/index.js"),
            "semantic_path"
        );
        assert_eq!(
            external_import_proof_label("forced-external:package-root:pkg@1.0.0/index.js"),
            "package_root"
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
