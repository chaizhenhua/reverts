use std::collections::BTreeSet;

use reverts_input::{InputRows, ModuleInput};
use reverts_ir::hash::fnv1a_hex as stable_hash;
use reverts_package::ExternalImportProofPath;

use crate::index::ExternalImportSourceIndex;
#[cfg(test)]
use crate::model::PackageSource;
use crate::model::{
    ExternalImportTarget, ModuleMatchStrategy, PackageMatch, PackageVersionCandidate,
    VersionedPackageMatcherConfig,
};
use crate::package_helpers::{
    SemanticPathHintMode, module_package_semantic_path_hints, package_source_external_import_rank,
    package_source_semantic_surface_hint_score,
};
use crate::scoring::best_source_match;
use crate::source::source_text::normalize_source;

use super::export_member::{
    export_member_external_package_source, export_member_external_package_source_for_source_path,
    public_export_member_external_package_source,
};
use super::policy::{
    SemanticExternalTargetPolicy, canonical_subpath_policy_allows, exact_hint_has_quality,
    semantic_external_source_proof_label, semantic_external_source_proof_rank,
    semantic_external_target_policies, semantic_source_only_export_member_policy_allows,
};
use super::scratch::ExternalImportProofScratch;
use super::semantic::{
    SemanticExternalSourceProof, disambiguate_semantic_build_variant_source,
    exact_hint_semantic_path, semantic_external_source_score,
    semantic_source_only_external_source_score, trusted_exact_generated_filename_hint,
};

pub(crate) fn importable_package_source_for_module(
    module: &ModuleInput,
    package_match: &PackageMatch,
    external_source_index: &ExternalImportSourceIndex<'_>,
    module_source: &str,
) -> Option<ExternalImportTarget> {
    resolve_external_import_target_with_index(
        module,
        package_match.package_name.as_str(),
        package_match.package_version.as_str(),
        Some(package_match),
        external_source_index,
        module_source,
    )
}

#[cfg(test)]
pub(crate) fn resolve_external_import_target(
    module: &ModuleInput,
    package_name: &str,
    package_version: &str,
    package_match: Option<&PackageMatch>,
    package_sources: &[PackageSource],
    module_source: &str,
) -> Option<ExternalImportTarget> {
    let external_source_index = ExternalImportSourceIndex::build(package_sources);
    resolve_external_import_target_with_index(
        module,
        package_name,
        package_version,
        package_match,
        &external_source_index,
        module_source,
    )
}

fn resolve_external_import_target_with_index(
    module: &ModuleInput,
    package_name: &str,
    package_version: &str,
    package_match: Option<&PackageMatch>,
    external_source_index: &ExternalImportSourceIndex<'_>,
    module_source: &str,
) -> Option<ExternalImportTarget> {
    let cache = ExternalImportProofScratch::default();
    if let Some(target) = normalized_source_external_package_source(
        module,
        package_name,
        package_version,
        external_source_index,
        module_source,
    ) {
        return Some(target);
    }

    if let Some(package_match) = package_match
        && let Some(target) =
            exact_importable_package_match_source(package_match, external_source_index)
    {
        return Some(target);
    }

    if let Some(package_match) = package_match
        && let Some(target) = public_export_member_external_package_source(
            module,
            package_match,
            external_source_index,
            module_source,
        )
    {
        return Some(target);
    }

    if let Some(package_match) = package_match
        && let Some(target) = dependency_exact_hint_source_match_external_package_source(
            module,
            package_match,
            external_source_index,
            module_source,
            &cache,
        )
    {
        return Some(target);
    }

    if let Some(package_match) = package_match
        && let Some(target) = export_member_external_package_source(
            package_match,
            external_source_index,
            module_source,
        )
    {
        return Some(target);
    }

    if let Some(package_match) = package_match
        && let Some(target) = canonical_subpath_external_package_source(
            module,
            package_match,
            external_source_index,
            module_source,
        )
    {
        return Some(target);
    }

    let semantic_policies = package_match
        .map(semantic_external_target_policies)
        .unwrap_or_default();
    if semantic_policies.is_empty() {
        return None;
    }

    for semantic_policy in semantic_policies {
        let hints = semantic_external_target_hints(
            package_name,
            module.semantic_path.as_str(),
            package_match,
            module_source,
            semantic_policy,
        );
        if let Some(target) = semantic_external_package_source(
            package_name,
            package_version,
            external_source_index,
            hints.as_slice(),
            semantic_policy.min_score,
        ) {
            return Some(target);
        }
        if let Some(package_match) = package_match
            && let Some(target) = semantic_source_only_export_member_package_source(
                package_match,
                external_source_index,
                hints.as_slice(),
                semantic_policy.min_score,
                module_source,
            )
        {
            return Some(target);
        }
    }
    None
}

fn semantic_external_target_hints(
    package_name: &str,
    module_semantic_path: &str,
    package_match: Option<&PackageMatch>,
    module_source: &str,
    semantic_policy: SemanticExternalTargetPolicy,
) -> Vec<String> {
    let mut hints = module_package_semantic_path_hints(
        package_name,
        module_semantic_path,
        module_source,
        semantic_policy.hint_mode,
    );
    if let Some(package_match) = package_match
        && let Some(exact_path) = exact_hint_semantic_path(package_match.source_path.as_str())
    {
        hints.extend(module_package_semantic_path_hints(
            package_match.package_name.as_str(),
            exact_path.as_str(),
            module_source,
            semantic_policy.hint_mode,
        ));
        if let Some(hint) = trusted_exact_generated_filename_hint(
            package_match,
            exact_path.as_str(),
            semantic_policy.hint_mode,
        ) {
            hints.push(hint);
        }
    }
    hints.sort();
    hints.dedup();
    hints
}

fn dependency_exact_hint_source_match_external_package_source<'a>(
    module: &ModuleInput,
    package_match: &PackageMatch,
    external_source_index: &ExternalImportSourceIndex<'a>,
    module_source: &str,
    cache: &ExternalImportProofScratch<'a>,
) -> Option<ExternalImportTarget> {
    if package_match.strategy != ModuleMatchStrategy::DependencyClosureOwnership
        || !exact_hint_has_quality(package_match, "trusted")
        || module_source.trim().is_empty()
    {
        return None;
    }
    let module_fingerprint =
        cache.module_fingerprint(module, module.semantic_path.as_str(), module_source)?;
    let sources = cache.source_fingerprints_for_version(
        external_source_index,
        package_match.package_name.as_str(),
        package_match.package_version.as_str(),
    );
    if sources.is_empty() {
        return None;
    }
    let version = PackageVersionCandidate {
        package_name: package_match.package_name.clone(),
        package_version: package_match.package_version.clone(),
        sources,
    };
    let source_match = best_source_match(
        &version,
        &module_fingerprint,
        &VersionedPackageMatcherConfig::default(),
    )?;
    match source_match.strategy {
        ModuleMatchStrategy::NormalizedSourceHash
        | ModuleMatchStrategy::FunctionSignatureAndStringAnchors
        | ModuleMatchStrategy::PropertyShapeAndStringAnchors
        | ModuleMatchStrategy::ObjectShapeAndStringAnchors
        | ModuleMatchStrategy::ClassShapeAndStringAnchors
        | ModuleMatchStrategy::SwitchShapeAndStringAnchors => {}
        ModuleMatchStrategy::AggregateFunctionSignatureAndStringAnchors
        | ModuleMatchStrategy::CascadeFunctionCoverage
        | ModuleMatchStrategy::CascadeFunctionOwnership
        | ModuleMatchStrategy::CascadePartialFunctionCoverage
        | ModuleMatchStrategy::AggregateStructuralBagSimilarity
        | ModuleMatchStrategy::AggregateStringAnchorSimilarity
        | ModuleMatchStrategy::DependencyClosureOwnership
        | ModuleMatchStrategy::PackageGraphNeighborhoodOwnership => return None,
    }
    if source_match.external_importable {
        return Some(ExternalImportTarget {
            export_specifier: source_match.export_specifier,
            source_path: ExternalImportProofPath::source_match(source_match.source_path.as_str()),
        });
    }
    export_member_external_package_source_for_source_path(
        source_match.package_name.as_str(),
        source_match.package_version.as_str(),
        source_match.source_path.as_str(),
        external_source_index,
        module_source,
    )
}

fn semantic_external_package_source(
    package_name: &str,
    package_version: &str,
    external_source_index: &ExternalImportSourceIndex<'_>,
    hints: &[String],
    min_score: usize,
) -> Option<ExternalImportTarget> {
    if hints.is_empty() {
        return None;
    }
    let mut scored = external_source_index
        .sources(package_name, package_version)
        .iter()
        .copied()
        .filter_map(|source| {
            let (score, proof) = hints
                .iter()
                .map(|hint| semantic_external_source_score(source, hint))
                .max_by(|left, right| {
                    left.0.cmp(&right.0).then_with(|| {
                        semantic_external_source_proof_rank(left.1)
                            .cmp(&semantic_external_source_proof_rank(right.1))
                    })
                })
                .unwrap_or((0, SemanticExternalSourceProof::SourcePath));
            (score >= min_score).then_some((source, score, proof))
        })
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| {
                semantic_external_source_proof_rank(right.2)
                    .cmp(&semantic_external_source_proof_rank(left.2))
            })
            .then_with(|| left.0.export_specifier.cmp(&right.0.export_specifier))
            .then_with(|| left.0.source_path.cmp(&right.0.source_path))
    });
    let best_score = scored.first()?.1;
    let best_proof = scored.first()?.2;
    let best = scored
        .into_iter()
        .filter(|(_source, score, proof)| *score == best_score && *proof == best_proof)
        .map(|(source, _score, _proof)| source)
        .collect::<Vec<_>>();
    let export_specifiers = best
        .iter()
        .map(|source| source.export_specifier.as_str())
        .collect::<BTreeSet<_>>();
    if export_specifiers.len() != 1 {
        let source = disambiguate_semantic_build_variant_source(best.as_slice())?;
        return Some(ExternalImportTarget {
            export_specifier: source.export_specifier.clone(),
            source_path: ExternalImportProofPath::semantic_build_variant(
                semantic_external_source_proof_label(best_proof),
                source.source_path.as_str(),
            ),
        });
    }
    let export_specifier = export_specifiers.into_iter().next()?;
    let source = best.into_iter().min_by(|left, right| {
        package_source_external_import_rank(left)
            .cmp(&package_source_external_import_rank(right))
            .then_with(|| left.source_path.cmp(&right.source_path))
    })?;
    Some(ExternalImportTarget {
        export_specifier: export_specifier.to_string(),
        source_path: ExternalImportProofPath::semantic_source(
            semantic_external_source_proof_label(best_proof),
            source.source_path.as_str(),
        ),
    })
}

fn canonical_subpath_external_package_source(
    module: &ModuleInput,
    package_match: &PackageMatch,
    external_source_index: &ExternalImportSourceIndex<'_>,
    module_source: &str,
) -> Option<ExternalImportTarget> {
    if !canonical_subpath_policy_allows(package_match) {
        return None;
    }
    let mut hints = module_package_semantic_path_hints(
        package_match.package_name.as_str(),
        module.semantic_path.as_str(),
        module_source,
        SemanticPathHintMode::RelaxedImportProof,
    );
    if let Some(exact_hint) = exact_hint_semantic_path(package_match.source_path.as_str()) {
        hints.extend(module_package_semantic_path_hints(
            package_match.package_name.as_str(),
            exact_hint.as_str(),
            module_source,
            SemanticPathHintMode::RelaxedImportProof,
        ));
    }
    hints.sort();
    hints.dedup();
    let mut scored = external_source_index
        .sources(
            package_match.package_name.as_str(),
            package_match.package_version.as_str(),
        )
        .iter()
        .copied()
        .filter_map(|source| {
            let best_score = hints
                .iter()
                .map(|hint| package_source_semantic_surface_hint_score(source, hint))
                .max()
                .unwrap_or(0);
            (best_score >= 5).then_some((source, best_score))
        })
        .collect::<Vec<_>>();
    if scored.is_empty() {
        return None;
    }
    scored.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| {
                package_source_external_import_rank(left.0)
                    .cmp(&package_source_external_import_rank(right.0))
            })
            .then_with(|| left.0.export_specifier.cmp(&right.0.export_specifier))
            .then_with(|| left.0.source_path.cmp(&right.0.source_path))
    });
    let best_score = scored.first()?.1;
    let best = scored
        .into_iter()
        .filter(|(_source, score)| *score == best_score)
        .map(|(source, _score)| source)
        .collect::<Vec<_>>();
    let export_specifiers = best
        .iter()
        .map(|source| source.export_specifier.as_str())
        .collect::<BTreeSet<_>>();
    if export_specifiers.len() != 1 {
        return None;
    }
    let export_specifier = export_specifiers.into_iter().next()?;
    let source = best.into_iter().min_by(|left, right| {
        package_source_external_import_rank(left)
            .cmp(&package_source_external_import_rank(right))
            .then_with(|| left.source_path.cmp(&right.source_path))
    })?;
    Some(ExternalImportTarget {
        export_specifier: export_specifier.to_string(),
        source_path: ExternalImportProofPath::canonical_subpath(source.source_path.as_str()),
    })
}

fn semantic_source_only_export_member_package_source(
    package_match: &PackageMatch,
    external_source_index: &ExternalImportSourceIndex<'_>,
    hints: &[String],
    min_score: usize,
    module_source: &str,
) -> Option<ExternalImportTarget> {
    if hints.is_empty()
        || !semantic_source_only_export_member_policy_allows(package_match)
        || module_source.trim().is_empty()
    {
        return None;
    }
    // Importable sources were already handled by semantic_external_package_source.
    // For source-only package files, require at least a structured suffix/path
    // match and then separately prove that a public import surface re-exports
    // the matched members.
    let min_score = if package_match.source_path.contains(":quality=trusted:") && min_score <= 1 {
        3
    } else {
        min_score.max(4)
    };
    let mut scored = external_source_index
        .all_sources(
            package_match.package_name.as_str(),
            package_match.package_version.as_str(),
        )
        .iter()
        .copied()
        .filter(|source| !source.external_importable)
        .filter_map(|source| {
            let export_members = external_source_index.export_members(source);
            let (score, proof) = hints
                .iter()
                .map(|hint| {
                    semantic_source_only_external_source_score(source, &export_members, hint)
                })
                .max_by(|left, right| {
                    left.0.cmp(&right.0).then_with(|| {
                        semantic_external_source_proof_rank(left.1)
                            .cmp(&semantic_external_source_proof_rank(right.1))
                    })
                })
                .unwrap_or((0, SemanticExternalSourceProof::SourcePath));
            (score >= min_score).then_some((source, score, proof))
        })
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| {
                semantic_external_source_proof_rank(right.2)
                    .cmp(&semantic_external_source_proof_rank(left.2))
            })
            .then_with(|| left.0.source_path.cmp(&right.0.source_path))
            .then_with(|| left.0.export_specifier.cmp(&right.0.export_specifier))
    });
    let best_score = scored.first()?.1;
    let best_proof = scored.first()?.2;
    let best = scored
        .into_iter()
        .filter(|(_source, score, proof)| *score == best_score && *proof == best_proof)
        .map(|(source, _score, _proof)| source)
        .collect::<Vec<_>>();
    let targets = best
        .into_iter()
        .filter_map(|source| {
            export_member_external_package_source_for_source_path(
                package_match.package_name.as_str(),
                package_match.package_version.as_str(),
                source.source_path.as_str(),
                external_source_index,
                module_source,
            )
        })
        .map(|target| (target.export_specifier, target.source_path))
        .collect::<BTreeSet<_>>();
    if targets.len() != 1 {
        return None;
    }
    let (export_specifier, source_path) = targets.into_iter().next()?;
    Some(ExternalImportTarget {
        export_specifier,
        source_path,
    })
}

fn exact_importable_package_match_source(
    package_match: &PackageMatch,
    external_source_index: &ExternalImportSourceIndex<'_>,
) -> Option<ExternalImportTarget> {
    if package_match.strategy != ModuleMatchStrategy::NormalizedSourceHash
        || package_match.normalized_source_hash.trim().is_empty()
    {
        return None;
    }
    let sources = external_source_index.normalized_sources(
        package_match.package_name.as_str(),
        package_match.package_version.as_str(),
        package_match.normalized_source_hash.as_str(),
    );
    let exact_source_paths = sources
        .iter()
        .copied()
        .filter(|source| source.source_path == package_match.source_path)
        .map(|source| {
            (
                source.export_specifier.as_str(),
                source.source_path.as_str(),
            )
        })
        .collect::<BTreeSet<_>>();
    if exact_source_paths.len() == 1 {
        let (export_specifier, source_path) = exact_source_paths.into_iter().next()?;
        return Some(ExternalImportTarget {
            export_specifier: export_specifier.to_string(),
            source_path: source_path.to_string(),
        });
    }
    None
}

fn normalized_source_external_package_source(
    module: &ModuleInput,
    package_name: &str,
    package_version: &str,
    external_source_index: &ExternalImportSourceIndex<'_>,
    module_source: &str,
) -> Option<ExternalImportTarget> {
    if module_source.trim().is_empty() {
        return None;
    }
    let normalized = normalize_source(module.semantic_path.as_str(), module_source).ok()?;
    let normalized_hash = stable_hash(normalized.as_bytes());
    let candidates = external_source_index.normalized_sources(
        package_name,
        package_version,
        normalized_hash.as_str(),
    );
    let best = *candidates.first()?;
    let best_key = package_source_external_import_rank(best);
    if candidates.get(1).is_some_and(|candidate| {
        package_source_external_import_rank(candidate) == best_key
            && candidate.export_specifier != best.export_specifier
    }) {
        return None;
    }
    Some(ExternalImportTarget {
        export_specifier: best.export_specifier.clone(),
        source_path: ExternalImportProofPath::normalized_source_export(best.source_path.as_str()),
    })
}

pub(crate) fn proven_external_package_version(
    module: &ModuleInput,
    source_only_match: Option<&PackageMatch>,
) -> Option<String> {
    module
        .package_version
        .as_deref()
        .map(str::trim)
        .filter(|version| !version.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| source_only_match.map(|package_match| package_match.package_version.clone()))
}

pub(crate) fn proven_external_import_target(
    rows: &InputRows,
    module: &ModuleInput,
    package_name: &str,
    package_version: &str,
    source_only_match: Option<&PackageMatch>,
    external_source_index: &ExternalImportSourceIndex<'_>,
) -> Option<ExternalImportTarget> {
    let module_source = rows
        .module_source_slice(module.id)
        .map(|slice| slice.source)
        .unwrap_or_default();
    resolve_external_import_target_with_index(
        module,
        package_name,
        package_version,
        source_only_match,
        external_source_index,
        module_source,
    )
}
