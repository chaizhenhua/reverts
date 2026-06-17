use std::collections::{BTreeMap, BTreeSet};

use reverts_input::ModuleInput;
use reverts_ir::hash::fnv1a_hex as stable_hash;

use crate::binding_signatures::binding_string_signatures_from_source;
use crate::index::{fingerprint_source, is_strong_path_hint_token};
use crate::package_helpers::{
    module_package_semantic_path_hints, package_source_entry_path,
    package_source_external_import_rank, strip_source_extension,
};
use crate::source::exported_members::{
    export_member_set_is_strong, exported_members_from_source, is_identifier_name,
    is_usable_export_member,
};
use crate::source::import_targets::{commonjs_reexport_targets, export_all_reexport_targets};
use crate::source::package_refs::{
    package_source_cache_key, package_source_export_all_reexport_entries,
    package_source_reexport_entries, relative_require_targets_package_source,
    source_entry_paths_match,
};
use crate::{
    ExternalImportSourceIndex, ExternalImportTarget, PACKAGE_SOURCE_FINGERPRINT_MAX_BYTES,
    PackageMatch, PackageSource, normalize_source,
};

use super::policy::{
    export_member_source_proof_alias_source_is_matched, export_member_source_proof_label,
    export_member_source_proof_rank, public_export_member_policy_allows,
    semantic_external_target_policies, source_only_match_can_be_promoted_to_import,
};
use super::semantic::semantic_external_source_score;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ExportMemberSourceProof {
    BarrelReference,
    BuildVariantPeer,
    CommonJsReexport,
    ExportAllReexport,
    NamedReexport,
    SourceEquivalent,
}

pub(crate) fn public_export_member_signature_score(
    module_source: &str,
    external_source: &str,
    module_members: &BTreeSet<String>,
) -> usize {
    let module_signatures = binding_string_signatures_from_source(module_source);
    let external_signatures = binding_string_signatures_from_source(external_source);
    module_members
        .iter()
        .filter_map(|member| {
            let module_signature = module_signatures.get(member)?;
            let external_signature = external_signatures.get(member)?;
            let overlap = module_signature.intersection(external_signature).count();
            if overlap > 0 {
                return Some(1_000 + overlap);
            }
            export_member_alias_score(module_signature, member, external_signature)
        })
        .max()
        .unwrap_or(0)
}

pub(crate) fn export_member_proof_source_path(
    source: &PackageSource,
    proof: ExportMemberSourceProof,
    members: &BTreeSet<String>,
    aliases: &BTreeMap<String, String>,
) -> String {
    let members = members
        .iter()
        .take(64)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(",");
    let alias_proof = export_member_alias_proof_fragment(aliases);
    if !alias_proof.is_empty() {
        return format!(
            "forced-external:export-members:{}:{}:aliases={}:{}",
            export_member_source_proof_label(proof),
            members,
            alias_proof,
            source.source_path
        );
    }
    format!(
        "forced-external:export-members:{}:{}:{}",
        export_member_source_proof_label(proof),
        members,
        source.source_path
    )
}

fn export_member_alias_proof_fragment(aliases: &BTreeMap<String, String>) -> String {
    aliases
        .iter()
        .take(64)
        .filter(|(local, exported)| {
            local.as_str() != exported.as_str()
                && is_identifier_name(local.as_str())
                && is_identifier_name(exported.as_str())
        })
        .map(|(local, exported)| format!("{local}={exported}"))
        .collect::<Vec<_>>()
        .join(",")
}

pub(crate) fn export_member_proof_fragment(members: &BTreeSet<String>) -> String {
    members
        .iter()
        .take(64)
        .filter(|member| is_identifier_name(member.as_str()))
        .cloned()
        .collect::<Vec<_>>()
        .join(",")
}

pub(crate) fn export_member_alias_proof_map(
    module_source: &str,
    external_source: &str,
    exported_members: &BTreeSet<String>,
) -> BTreeMap<String, String> {
    if module_source.trim().is_empty() || exported_members.is_empty() {
        return BTreeMap::new();
    }
    let local_signatures = binding_string_signatures_from_source(module_source);
    let external_signatures = binding_string_signatures_from_source(external_source)
        .into_iter()
        .filter(|(binding, signature)| {
            exported_members.contains(binding.as_str()) && !signature.is_empty()
        })
        .collect::<BTreeMap<_, _>>();
    if local_signatures.is_empty() || external_signatures.is_empty() {
        return BTreeMap::new();
    }

    let mut aliases = BTreeMap::new();
    for (local, local_signature) in local_signatures {
        if exported_members.contains(local.as_str()) || local_signature.is_empty() {
            continue;
        }
        let mut scored = external_signatures
            .iter()
            .filter_map(|(exported, external_signature)| {
                export_member_alias_score(&local_signature, exported.as_str(), external_signature)
                    .map(|score| (exported.as_str(), score))
            })
            .collect::<Vec<_>>();
        scored.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
        let Some((best_exported, best_score)) = scored.first().copied() else {
            continue;
        };
        if scored
            .get(1)
            .is_some_and(|(_exported, score)| *score == best_score)
        {
            continue;
        }
        aliases.insert(local, best_exported.to_string());
    }
    aliases
}

fn export_member_alias_score(
    local_signature: &BTreeSet<String>,
    exported_member: &str,
    external_signature: &BTreeSet<String>,
) -> Option<usize> {
    if local_signature.contains(exported_member) && external_signature.contains(exported_member) {
        return Some(10_000 + local_signature.intersection(external_signature).count());
    }
    let overlap = local_signature.intersection(external_signature).count();
    if overlap < 3 {
        return None;
    }
    let smaller = local_signature.len().min(external_signature.len());
    (overlap * 100 >= smaller * 80).then_some(1_000 + overlap)
}

#[derive(Debug, Clone, Copy)]
struct ExportMemberExternalCandidate<'a> {
    external: &'a PackageSource,
    matched: &'a PackageSource,
    proof: ExportMemberSourceProof,
}

pub(crate) fn export_member_external_package_source(
    package_match: &PackageMatch,
    external_source_index: &ExternalImportSourceIndex<'_>,
    module_source: &str,
) -> Option<ExternalImportTarget> {
    if !source_only_match_can_be_promoted_to_import(package_match.strategy) {
        return None;
    }
    export_member_external_package_source_for_source_path(
        package_match.package_name.as_str(),
        package_match.package_version.as_str(),
        package_match.source_path.as_str(),
        external_source_index,
        module_source,
    )
}

pub(crate) fn public_export_member_external_package_source(
    module: &ModuleInput,
    package_match: &PackageMatch,
    external_source_index: &ExternalImportSourceIndex<'_>,
    module_source: &str,
) -> Option<ExternalImportTarget> {
    if module_source.trim().is_empty() || !public_export_member_policy_allows(package_match) {
        return None;
    }
    let module_members = exported_members_from_source(module.semantic_path.as_str(), module_source);
    if !export_member_set_is_strong(module_members.iter()) {
        return None;
    }
    let semantic_policies = semantic_external_target_policies(package_match);
    if semantic_policies.is_empty() {
        return None;
    }
    let mut candidates = external_source_index
        .sources(
            package_match.package_name.as_str(),
            package_match.package_version.as_str(),
        )
        .iter()
        .copied()
        .filter(|source| {
            let external_members = external_source_index.export_members(source);
            module_members.is_subset(&external_members)
                && export_member_set_is_strong(external_members.iter())
        })
        .filter_map(|source| {
            let best_semantic_score = semantic_policies
                .iter()
                .filter_map(|policy| {
                    let hints = module_package_semantic_path_hints(
                        package_match.package_name.as_str(),
                        module.semantic_path.as_str(),
                        module_source,
                        policy.hint_mode,
                    );
                    hints
                        .iter()
                        .map(|hint| semantic_external_source_score(source, hint).0)
                        .max()
                        .filter(|score| *score >= policy.min_score)
                })
                .max()
                .unwrap_or(0);
            let public_member_score = public_export_member_signature_score(
                module_source,
                source.source.as_str(),
                &module_members,
            );
            (best_semantic_score > 0 || public_member_score > 0).then_some((
                source,
                best_semantic_score,
                public_member_score,
            ))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .2
            .cmp(&left.2)
            .then_with(|| right.1.cmp(&left.1))
            .then_with(|| {
                package_source_external_import_rank(left.0)
                    .cmp(&package_source_external_import_rank(right.0))
            })
            .then_with(|| left.0.export_specifier.cmp(&right.0.export_specifier))
            .then_with(|| left.0.source_path.cmp(&right.0.source_path))
    });
    let best_signature_score = candidates.first()?.2;
    let best_semantic_score = candidates.first()?.1;
    let best_rank = package_source_external_import_rank(candidates.first()?.0);
    let best = candidates
        .into_iter()
        .filter(|(source, semantic_score, signature_score)| {
            *signature_score == best_signature_score
                && *semantic_score == best_semantic_score
                && package_source_external_import_rank(source) == best_rank
        })
        .collect::<Vec<_>>();
    let export_specifiers = best
        .iter()
        .map(|(source, _, _)| source.export_specifier.as_str())
        .collect::<BTreeSet<_>>();
    if export_specifiers.len() != 1 {
        return None;
    }
    let export_specifier = export_specifiers.into_iter().next()?;
    let source = best.into_iter().min_by(|left, right| {
        package_source_external_import_rank(left.0)
            .cmp(&package_source_external_import_rank(right.0))
            .then_with(|| left.0.source_path.cmp(&right.0.source_path))
    })?;
    Some(ExternalImportTarget {
        export_specifier: export_specifier.to_string(),
        source_path: format!(
            "forced-external:public-export-members:members={}:{}",
            export_member_proof_fragment(&module_members),
            source.0.source_path
        ),
    })
}

pub(crate) fn export_member_external_package_source_for_source_path(
    package_name: &str,
    package_version: &str,
    matched_source_path: &str,
    external_source_index: &ExternalImportSourceIndex<'_>,
    module_source: &str,
) -> Option<ExternalImportTarget> {
    let matched_sources = external_source_index.all_sources_by_path(
        package_name,
        package_version,
        matched_source_path,
    );
    if matched_sources.is_empty() {
        return None;
    }

    let matched_members = matched_sources
        .iter()
        .flat_map(|source| external_source_index.export_members(source))
        .filter(|member| is_usable_export_member(member))
        .collect::<BTreeSet<_>>();
    if !export_member_set_is_strong(matched_members.iter()) {
        return None;
    }

    let mut candidates = Vec::<ExportMemberExternalCandidate<'_>>::new();
    for external in external_source_index.sources(package_name, package_version) {
        let external_members = external_source_index.export_members(external);
        let Some((matched, proof)) = matched_sources
            .iter()
            .filter_map(|matched| {
                let proof = export_member_source_proof(
                    matched,
                    external,
                    &matched_members,
                    &external_members,
                    external_source_index,
                )?;
                Some((*matched, proof))
            })
            .max_by(|left, right| {
                export_member_source_proof_rank(left.1)
                    .cmp(&export_member_source_proof_rank(right.1))
            })
        else {
            continue;
        };
        candidates.push(ExportMemberExternalCandidate {
            external,
            matched,
            proof,
        });
    }
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by(|left, right| {
        export_member_source_proof_rank(right.proof)
            .cmp(&export_member_source_proof_rank(left.proof))
            .then_with(|| {
                package_source_external_import_rank(left.external)
                    .cmp(&package_source_external_import_rank(right.external))
            })
            .then_with(|| {
                left.external
                    .export_specifier
                    .cmp(&right.external.export_specifier)
            })
            .then_with(|| left.external.source_path.cmp(&right.external.source_path))
            .then_with(|| left.matched.source_path.cmp(&right.matched.source_path))
    });
    let best_proof = candidates.first()?.proof;
    let best_rank = package_source_external_import_rank(candidates.first()?.external);
    let best = candidates
        .into_iter()
        .filter(|candidate| {
            candidate.proof == best_proof
                && package_source_external_import_rank(candidate.external) == best_rank
        })
        .collect::<Vec<_>>();
    let export_specifiers = best
        .iter()
        .map(|candidate| candidate.external.export_specifier.as_str())
        .collect::<BTreeSet<_>>();
    if export_specifiers.len() != 1 {
        return None;
    }
    let export_specifier = export_specifiers.into_iter().next()?;
    let source = best.into_iter().min_by(|left, right| {
        package_source_external_import_rank(left.external)
            .cmp(&package_source_external_import_rank(right.external))
            .then_with(|| left.external.source_path.cmp(&right.external.source_path))
            .then_with(|| left.matched.source_path.cmp(&right.matched.source_path))
    })?;
    let alias_source = if export_member_source_proof_alias_source_is_matched(best_proof) {
        source.matched
    } else {
        source.external
    };
    let alias_members = if export_member_source_proof_alias_source_is_matched(best_proof) {
        matched_members.clone()
    } else {
        external_source_index.export_members(alias_source)
    };
    let aliases =
        export_member_alias_proof_map(module_source, alias_source.source.as_str(), &alias_members);
    Some(ExternalImportTarget {
        export_specifier: export_specifier.to_string(),
        source_path: export_member_proof_source_path(
            source.external,
            best_proof,
            &matched_members,
            &aliases,
        ),
    })
}

fn export_member_source_proof(
    matched: &PackageSource,
    external: &PackageSource,
    matched_members: &BTreeSet<String>,
    external_members: &BTreeSet<String>,
    external_source_index: &ExternalImportSourceIndex<'_>,
) -> Option<ExportMemberSourceProof> {
    if package_sources_are_equivalent(matched, external) {
        return Some(ExportMemberSourceProof::SourceEquivalent);
    }
    if export_member_build_variant_peer(matched, external)
        && matched_members == external_members
        && export_member_set_is_strong(matched_members.iter())
    {
        return Some(ExportMemberSourceProof::BuildVariantPeer);
    }
    if matched_members.is_subset(external_members)
        && external_source_references_matched_member_source(external, matched)
    {
        return Some(ExportMemberSourceProof::BarrelReference);
    }
    if external_source_commonjs_reexports_matched_source(external, matched) {
        return Some(ExportMemberSourceProof::CommonJsReexport);
    }
    if external_source_export_all_reexports_matched_source(external, matched) {
        return Some(ExportMemberSourceProof::ExportAllReexport);
    }
    if external_source_export_all_reexports_matched_source_transitively(
        external,
        matched,
        external_source_index,
    ) {
        return Some(ExportMemberSourceProof::ExportAllReexport);
    }
    if external_source_reexports_matched_source_transitively(
        external,
        matched,
        external_source_index,
    ) {
        return Some(ExportMemberSourceProof::NamedReexport);
    }
    None
}

fn package_sources_are_equivalent(left: &PackageSource, right: &PackageSource) -> bool {
    if left.source == right.source {
        return true;
    }
    if let (Ok(left_normalized), Ok(right_normalized)) = (
        normalize_source(left.source_path.as_str(), left.source.as_str()),
        normalize_source(right.source_path.as_str(), right.source.as_str()),
    ) && stable_hash(left_normalized.as_bytes()) == stable_hash(right_normalized.as_bytes())
    {
        return true;
    }
    if left.source.len() > PACKAGE_SOURCE_FINGERPRINT_MAX_BYTES
        || right.source.len() > PACKAGE_SOURCE_FINGERPRINT_MAX_BYTES
    {
        return false;
    }
    let (Ok(left_fingerprint), Ok(right_fingerprint)) = (
        fingerprint_source(left.source_path.as_str(), left.source.as_str()),
        fingerprint_source(right.source_path.as_str(), right.source.as_str()),
    ) else {
        return false;
    };
    let function_matches = left_fingerprint
        .function_signature_hashes
        .intersection(&right_fingerprint.function_signature_hashes)
        .count();
    let string_matches = left_fingerprint
        .string_anchors
        .intersection(&right_fingerprint.string_anchors)
        .count();
    function_matches >= 3 || (function_matches >= 2 && string_matches >= 1)
}

fn export_member_build_variant_peer(left: &PackageSource, right: &PackageSource) -> bool {
    package_source_variant_neutral_path(left) == package_source_variant_neutral_path(right)
}

fn package_source_variant_neutral_path(source: &PackageSource) -> String {
    let entry = strip_source_extension(package_source_entry_path(source).as_str())
        .trim_matches('/')
        .to_ascii_lowercase();
    entry
        .split('/')
        .filter(|segment| {
            !matches!(
                *segment,
                "dist-cjs"
                    | "dist-es"
                    | "dist-esm"
                    | "cjs"
                    | "commonjs"
                    | "esm"
                    | "es"
                    | "module"
                    | "modules"
            )
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn external_source_references_matched_member_source(
    external: &PackageSource,
    matched: &PackageSource,
) -> bool {
    let external_source = external.source.replace('\\', "/").to_ascii_lowercase();
    let matched_entry = strip_source_extension(package_source_entry_path(matched).as_str())
        .trim_matches('/')
        .to_ascii_lowercase();
    let leaf = matched_entry
        .rsplit('/')
        .next()
        .unwrap_or(matched_entry.as_str());
    let mut candidates = BTreeSet::new();
    if is_strong_path_hint_token(leaf) {
        candidates.insert(leaf.to_string());
    }
    let tail = matched_entry
        .rsplit('/')
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("/");
    if tail.len() >= 4 {
        candidates.insert(tail);
    }
    if matched_entry.len() >= 4 {
        candidates.insert(matched_entry);
    }
    candidates.into_iter().any(|candidate| {
        external_source_contains_path_reference(external_source.as_str(), candidate.as_str())
    })
}

fn external_source_contains_path_reference(source: &str, candidate: &str) -> bool {
    source.contains(format!("./{candidate}").as_str())
        || source.contains(format!("../{candidate}").as_str())
        || source.contains(format!("/{candidate}").as_str())
        || (candidate.contains('/') && source.contains(candidate))
}

fn external_source_commonjs_reexports_matched_source(
    external: &PackageSource,
    matched: &PackageSource,
) -> bool {
    commonjs_reexport_targets(external.source.as_str())
        .into_iter()
        .any(|target| relative_require_targets_package_source(external, target.as_str(), matched))
}

fn external_source_export_all_reexports_matched_source(
    external: &PackageSource,
    matched: &PackageSource,
) -> bool {
    export_all_reexport_targets(external.source.as_str())
        .into_iter()
        .any(|target| relative_require_targets_package_source(external, target.as_str(), matched))
}

fn external_source_export_all_reexports_matched_source_transitively(
    external: &PackageSource,
    matched: &PackageSource,
    external_source_index: &ExternalImportSourceIndex<'_>,
) -> bool {
    let matched_entry = package_source_entry_path(matched);
    let mut visited = BTreeSet::<String>::new();
    external_source_export_all_reexports_entry_transitively(
        external,
        matched_entry.as_str(),
        external_source_index,
        &mut visited,
    )
}

fn external_source_export_all_reexports_entry_transitively(
    source: &PackageSource,
    matched_entry: &str,
    external_source_index: &ExternalImportSourceIndex<'_>,
    visited: &mut BTreeSet<String>,
) -> bool {
    let source_key = format!(
        "{}@{}:{}",
        source.package_name, source.package_version, source.source_path
    );
    if !visited.insert(source_key) {
        return false;
    }
    for entry in package_source_export_all_reexport_entries(source) {
        if source_entry_paths_match(entry.as_str(), matched_entry) {
            return true;
        }
        for next in external_source_index.sources_matching_entry(
            source.package_name.as_str(),
            source.package_version.as_str(),
            entry.as_str(),
        ) {
            if external_source_export_all_reexports_entry_transitively(
                next,
                matched_entry,
                external_source_index,
                visited,
            ) {
                return true;
            }
        }
    }
    false
}

fn external_source_reexports_matched_source_transitively(
    external: &PackageSource,
    matched: &PackageSource,
    external_source_index: &ExternalImportSourceIndex<'_>,
) -> bool {
    let matched_entry = package_source_entry_path(matched);
    let mut visited = BTreeSet::<String>::new();
    external_source_reexports_entry_transitively(
        external,
        matched_entry.as_str(),
        external_source_index,
        &mut visited,
    )
}

fn external_source_reexports_entry_transitively(
    source: &PackageSource,
    matched_entry: &str,
    external_source_index: &ExternalImportSourceIndex<'_>,
    visited: &mut BTreeSet<String>,
) -> bool {
    let source_key = package_source_cache_key(source);
    if !visited.insert(source_key) {
        return false;
    }
    for entry in package_source_reexport_entries(source) {
        if source_entry_paths_match(entry.as_str(), matched_entry) {
            return true;
        }
        for next in external_source_index.sources_matching_entry(
            source.package_name.as_str(),
            source.package_version.as_str(),
            entry.as_str(),
        ) {
            if external_source_reexports_entry_transitively(
                next,
                matched_entry,
                external_source_index,
                visited,
            ) {
                return true;
            }
        }
    }
    false
}
