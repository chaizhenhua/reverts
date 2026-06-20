//! Package-level index over [`PackageSource`] candidates plus the small
//! helpers that turn one bundle module or one package source body into a
//! [`ModuleMatchFingerprint`] / [`PackageSourceFingerprint`]. Drives the
//! exact-version match path that lives behind [`VersionedPackageMatcher`].

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use reverts_input::{InputRows, ModuleInput};
use reverts_ir::ModuleKind;
use reverts_js::{ParseGoal, parse_options_for, source_type_candidates};
use reverts_observe::{AuditFinding, AuditReport, FindingCode};

use super::SourceFingerprint;
use crate::package_helpers::{
    normalize_hint_text, package_semantic_path_prefixes, path_hint_tokens,
    strip_package_prefix_from_semantic_path, strip_source_extension,
};
use crate::scoring::{compare_versions, score_version};
use crate::{
    BestVersionMatch, ModuleMatchFingerprint, ModulePackageMatch, PackageModuleSourceQuality,
    PackageSource, PackageSourceFingerprint, PackageVersionCandidate, VersionMatchScore,
    VersionedPackageMatcherConfig, build_source_evidence_profile,
    build_source_evidence_profile_with_fingerprint, has_accepted_attribution,
};

#[derive(Debug)]
pub(crate) struct PackageVersionIndex<'a> {
    packages: BTreeMap<String, Vec<PackageVersionCandidate<'a>>>,
}

impl<'a> PackageVersionIndex<'a> {
    pub(crate) fn build(package_sources: &'a [PackageSource], audit: &mut AuditReport) -> Self {
        let mut by_version = BTreeMap::<(String, String), Vec<PackageSourceFingerprint<'a>>>::new();
        for source in package_sources {
            if !source.is_within_fingerprint_budget() {
                continue;
            }
            match package_source_fingerprint(source) {
                Ok(fingerprint) => {
                    by_version
                        .entry((source.package_name.clone(), source.package_version.clone()))
                        .or_default()
                        .push(fingerprint);
                }
                Err(message) => {
                    audit.push(
                        AuditFinding::error(FindingCode::UnparseablePackageSource, message)
                            .with_module(source.source_path.clone())
                            .with_binding(format!(
                                "{}@{}",
                                source.package_name, source.package_version
                            )),
                    );
                }
            }
        }

        let mut packages = BTreeMap::<String, Vec<PackageVersionCandidate<'a>>>::new();
        for ((package_name, package_version), mut sources) in by_version {
            sources.sort_by(|left, right| {
                left.normalized_source_hash
                    .cmp(&right.normalized_source_hash)
                    .then_with(|| {
                        right
                            .source
                            .external_importable
                            .cmp(&left.source.external_importable)
                    })
                    .then_with(|| left.source.source_path.cmp(&right.source.source_path))
                    .then_with(|| {
                        left.source
                            .export_specifier
                            .cmp(&right.source.export_specifier)
                    })
            });
            packages
                .entry(package_name.clone())
                .or_default()
                .push(PackageVersionCandidate {
                    package_name,
                    package_version,
                    sources,
                });
        }

        for versions in packages.values_mut() {
            versions.sort_by(|left, right| {
                compare_versions(
                    left.package_version.as_str(),
                    right.package_version.as_str(),
                )
            });
        }

        Self { packages }
    }

    pub(crate) fn has_package_version(&self, package_name: &str, package_version: &str) -> bool {
        self.version_candidate(package_name, package_version)
            .is_some()
    }

    fn version_candidate(
        &self,
        package_name: &str,
        package_version: &str,
    ) -> Option<&PackageVersionCandidate<'a>> {
        self.packages
            .get(package_name)?
            .iter()
            .find(|candidate| candidate.package_version == package_version)
    }

    pub(crate) fn match_exact_version_for_package(
        &self,
        package_name: &str,
        package_version: &str,
        module_fingerprints: &[ModuleMatchFingerprint],
        config: &VersionedPackageMatcherConfig,
    ) -> BestVersionMatch {
        let Some(version) = self.version_candidate(package_name, package_version) else {
            return BestVersionMatch::NoMatch {
                package_name: package_name.to_string(),
                scores: Vec::new(),
            };
        };
        let mut scored = score_version(version, module_fingerprints, config);
        scored.score.binary_search_probes = 1;
        decision_from_single_scored_version(package_name, scored, config)
    }
}

#[derive(Debug)]
pub(crate) struct ScoredPackageVersion {
    pub(crate) score: VersionMatchScore,
    pub(crate) module_matches: Vec<ModulePackageMatch>,
}

fn decision_from_single_scored_version(
    package_name: &str,
    scored: ScoredPackageVersion,
    config: &VersionedPackageMatcherConfig,
) -> BestVersionMatch {
    let ScoredPackageVersion {
        score,
        module_matches,
    } = scored;
    if !score.has_evidence() {
        return BestVersionMatch::NoMatch {
            package_name: package_name.to_string(),
            scores: vec![score],
        };
    }
    if score.source_hash_matches == 0
        && (score.function_signature_matches < config.min_function_signature_matches
            || score.string_anchor_matches < config.min_string_anchor_matches)
    {
        return BestVersionMatch::InsufficientEvidence { score };
    }
    BestVersionMatch::Selected {
        score,
        module_matches,
    }
}

pub(crate) fn fingerprint_modules_for_package(
    rows: &InputRows,
    package_name: &str,
    audit: &mut AuditReport,
) -> Vec<ModuleMatchFingerprint> {
    let mut fingerprints = Vec::new();
    for module in rows.modules.iter().filter(|module| {
        module.kind == ModuleKind::Package
            && module.package_name.as_deref() == Some(package_name)
            && !has_accepted_attribution(rows, module.id)
    }) {
        let Some(slice) = rows.module_source_slice(module.id) else {
            audit.push(
                AuditFinding::error(
                    FindingCode::MissingPackageSource,
                    "package module has no real source slice for matching",
                )
                .with_module(module.id.0.to_string())
                .with_binding(module.original_name.clone()),
            );
            continue;
        };

        match package_module_source_quality(module, slice.source_file_path, slice.source) {
            PackageModuleSourceQuality::Trusted => {}
            PackageModuleSourceQuality::Weak | PackageModuleSourceQuality::Invalid => continue,
        }

        match module_match_fingerprint(module, slice.source_file_path, slice.source) {
            Ok(fingerprint) => fingerprints.push(fingerprint),
            Err(message) => {
                audit.push(
                    AuditFinding::error(FindingCode::UnparseablePackageSource, message)
                        .with_module(module.id.0.to_string())
                        .with_binding(module.original_name.clone()),
                );
            }
        }
    }
    fingerprints
}

#[must_use]
pub fn package_module_source_quality(
    module: &ModuleInput,
    source_path: &str,
    source: &str,
) -> PackageModuleSourceQuality {
    if source.trim().is_empty() || !package_module_source_parses(source_path, source) {
        return PackageModuleSourceQuality::Invalid;
    }
    let Some(package_name) = module.package_name.as_deref() else {
        return PackageModuleSourceQuality::Trusted;
    };
    let hint_tokens = package_semantic_path_tokens(package_name, module.semantic_path.as_str());
    if hint_tokens.is_empty() {
        return PackageModuleSourceQuality::Trusted;
    }
    let normalized_source = normalize_hint_text(source);
    if hint_tokens
        .iter()
        .any(|token| normalized_source.contains(token.as_str()))
    {
        PackageModuleSourceQuality::Trusted
    } else {
        PackageModuleSourceQuality::Weak
    }
}

fn package_module_source_parses(source_path: &str, source: &str) -> bool {
    let allocator = Allocator::default();
    for source_type in source_type_candidates(Some(Path::new(source_path)), ParseGoal::TypeScript) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if parsed.errors.is_empty() && !parsed.panicked {
            return true;
        }
    }
    false
}

fn package_semantic_path_tokens(package_name: &str, semantic_path: &str) -> BTreeSet<String> {
    let clean = semantic_path
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/')
        .replace('\\', "/");
    let mut tokens = BTreeSet::new();
    for prefix in package_semantic_path_prefixes(package_name) {
        let Some(rest) = strip_package_prefix_from_semantic_path(clean.as_str(), prefix.as_str())
        else {
            continue;
        };
        for token in path_hint_tokens(strip_source_extension(rest)) {
            if is_strong_path_hint_token(token.as_str()) {
                tokens.insert(normalize_hint_text(token.as_str()));
            }
        }
    }
    tokens
}

pub(crate) fn is_strong_path_hint_token(token: &str) -> bool {
    token.len() >= 4
        && !matches!(
            token,
            "node"
                | "node_modules"
                | "module"
                | "modules"
                | "internal"
                | "index"
                | "src"
                | "dist"
                | "lib"
                | "cjs"
                | "esm"
                | "mjs"
                | "umd"
                | "operators"
                | "observable"
        )
}

pub(crate) fn module_match_fingerprint(
    module: &ModuleInput,
    path: &str,
    source: &str,
) -> Result<ModuleMatchFingerprint, String> {
    let profile = build_source_evidence_profile(path, source)?;
    let source_fingerprint = profile.fingerprint;
    Ok(ModuleMatchFingerprint {
        module_id: module.id,
        package_name: module.package_name.clone(),
        package_version: module.package_version.clone(),
        normalized_source_hash: source_fingerprint.normalized_source_hash,
        normalized_source_hashes: source_fingerprint.normalized_source_hashes,
        function_signature_hashes: source_fingerprint.function_signature_hashes,
        top_level_declaration_hashes: source_fingerprint.top_level_declaration_hashes,
        import_export_surface_hashes: source_fingerprint.import_export_surface_hashes,
        class_member_hashes: source_fingerprint.class_member_hashes,
        statement_window_hashes: source_fingerprint.statement_window_hashes,
        block_branch_hashes: source_fingerprint.block_branch_hashes,
        pq_gram_hashes: source_fingerprint.pq_gram_hashes,
        string_anchors: source_fingerprint.string_anchors,
        function_axis_anchors: profile.function_axis_anchors,
        jsx_react_shape_anchors: profile.jsx_react_shape_anchors,
    })
}

pub(crate) fn package_source_fingerprint<'a>(
    source: &'a PackageSource,
) -> Result<PackageSourceFingerprint<'a>, String> {
    // Reuse a precomputed fingerprint when the source carries one — package
    // sources are immutable per (package, version, path), so this skips the
    // expensive parse + normalize + signature extraction on warm runs.
    if let Some(cached) = &source.fingerprint {
        let profile = build_source_evidence_profile_with_fingerprint(
            source.source_path.as_str(),
            source.source.as_str(),
            cached.clone(),
        );
        return Ok(PackageSourceFingerprint {
            source,
            normalized_source_hash: cached.normalized_source_hash.clone(),
            normalized_source_hashes: cached.normalized_source_hashes.clone(),
            function_signature_hashes: cached.function_signature_hashes.clone(),
            top_level_declaration_hashes: cached.top_level_declaration_hashes.clone(),
            import_export_surface_hashes: cached.import_export_surface_hashes.clone(),
            class_member_hashes: cached.class_member_hashes.clone(),
            statement_window_hashes: cached.statement_window_hashes.clone(),
            block_branch_hashes: cached.block_branch_hashes.clone(),
            pq_gram_hashes: cached.pq_gram_hashes.clone(),
            string_anchors: cached.string_anchors.clone(),
            function_axis_anchors: profile.function_axis_anchors,
            jsx_react_shape_anchors: profile.jsx_react_shape_anchors,
        });
    }
    let profile =
        build_source_evidence_profile(source.source_path.as_str(), source.source.as_str())?;
    Ok(package_source_fingerprint_from_source(
        source,
        profile.fingerprint,
        profile.function_axis_anchors,
        profile.jsx_react_shape_anchors,
    ))
}

pub(crate) fn package_source_fingerprint_from_source<'a>(
    source: &'a PackageSource,
    fingerprint: SourceFingerprint,
    function_axis_anchors: BTreeSet<String>,
    jsx_react_shape_anchors: BTreeSet<String>,
) -> PackageSourceFingerprint<'a> {
    PackageSourceFingerprint {
        source,
        normalized_source_hash: fingerprint.normalized_source_hash,
        normalized_source_hashes: fingerprint.normalized_source_hashes,
        function_signature_hashes: fingerprint.function_signature_hashes,
        top_level_declaration_hashes: fingerprint.top_level_declaration_hashes,
        import_export_surface_hashes: fingerprint.import_export_surface_hashes,
        class_member_hashes: fingerprint.class_member_hashes,
        statement_window_hashes: fingerprint.statement_window_hashes,
        block_branch_hashes: fingerprint.block_branch_hashes,
        pq_gram_hashes: fingerprint.pq_gram_hashes,
        string_anchors: fingerprint.string_anchors,
        function_axis_anchors,
        jsx_react_shape_anchors,
    }
}

#[cfg(test)]
mod fingerprint_cache_tests {
    use std::collections::BTreeSet;

    use super::package_source_fingerprint;
    use crate::{PackageSource, SourceFingerprint};

    fn anchors(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn reuses_cached_fingerprint_without_parsing() {
        let cached = SourceFingerprint {
            normalized_source_hash: "deadbeef".to_string(),
            normalized_source_hashes: anchors(&["deadbeef", "alt"]),
            function_signature_hashes: anchors(&["sig1"]),
            top_level_declaration_hashes: anchors(&["decl1"]),
            import_export_surface_hashes: anchors(&["surface1"]),
            class_member_hashes: anchors(&["member1"]),
            statement_window_hashes: anchors(&["window1"]),
            block_branch_hashes: anchors(&["block1"]),
            pq_gram_hashes: anchors(&["pq1"]),
            string_anchors: anchors(&["anchor"]),
        };
        // Deliberately unparseable body: if the matcher reused the cache it
        // returns the cached hashes verbatim; if it re-parsed it would error or
        // differ. This proves the warm path skips the parse entirely.
        let source =
            PackageSource::external("pkg", "1.0.0", "pkg", "index.js", "this is %%% $$$ broken")
                .with_fingerprint(cached.clone());
        let fp = package_source_fingerprint(&source)
            .expect("cached fingerprint must be reused without parsing the source");
        assert_eq!(fp.normalized_source_hash, cached.normalized_source_hash);
        assert_eq!(fp.normalized_source_hashes, cached.normalized_source_hashes);
        assert_eq!(
            fp.function_signature_hashes,
            cached.function_signature_hashes
        );
        assert_eq!(
            fp.top_level_declaration_hashes,
            cached.top_level_declaration_hashes
        );
        assert_eq!(
            fp.import_export_surface_hashes,
            cached.import_export_surface_hashes
        );
        assert_eq!(fp.class_member_hashes, cached.class_member_hashes);
        assert_eq!(fp.statement_window_hashes, cached.statement_window_hashes);
        assert_eq!(fp.block_branch_hashes, cached.block_branch_hashes);
        assert_eq!(fp.pq_gram_hashes, cached.pq_gram_hashes);
        assert_eq!(fp.string_anchors, cached.string_anchors);
    }

    #[test]
    fn computes_fingerprint_when_uncached() {
        let source =
            PackageSource::external("pkg", "1.0.0", "pkg", "index.js", "export const x = 1;\n");
        let fp = package_source_fingerprint(&source).expect("valid source fingerprints");
        assert!(!fp.normalized_source_hash.is_empty());
    }
}
