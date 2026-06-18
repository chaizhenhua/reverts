use std::collections::BTreeSet;

use crate::package_helpers::{
    canonical_public_path_segments, is_build_path_segment, normalize_hint_text,
    package_source_export_path, package_source_external_import_rank, package_source_relative_path,
    package_source_semantic_hint_score, path_hint_tokens, strip_source_extension,
};
use crate::{PackageMatch, PackageSource, SemanticPathHintMode};

use super::policy::{exact_hint_has_quality, semantic_external_source_proof_rank};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SemanticExternalSourceProof {
    SourcePath,
    ExportSurface,
    ExportMember,
}

pub(crate) fn exact_hint_semantic_path(source_path: &str) -> Option<String> {
    source_path
        .split(":semantic_path=")
        .nth(1)
        .map(|tail| tail.split(':').next().unwrap_or(tail))
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) fn trusted_exact_generated_filename_hint(
    package_match: &PackageMatch,
    exact_semantic_path: &str,
    hint_mode: SemanticPathHintMode,
) -> Option<String> {
    if !matches!(
        hint_mode,
        SemanticPathHintMode::ImportProof | SemanticPathHintMode::RelaxedImportProof
    ) || !exact_hint_has_quality(package_match, "trusted")
    {
        return None;
    }
    let stem = exact_semantic_path
        .strip_prefix("modules/")
        .map(strip_source_extension)
        .map(str::trim)?;
    let (prefix, rest) = stem.split_once('-')?;
    if prefix.is_empty() || !prefix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let hint = rest.trim_matches('/').to_ascii_lowercase();
    if !generated_filename_hint_is_public_member_bridge_candidate(hint.as_str()) {
        return None;
    }
    Some(hint)
}

fn generated_filename_hint_is_public_member_bridge_candidate(hint: &str) -> bool {
    let trimmed = hint.trim().trim_matches('/');
    if trimmed.is_empty() || trimmed.contains('/') {
        return false;
    }
    let tokens = path_hint_tokens(trimmed);
    if tokens.len() < 2 {
        return false;
    }
    if tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            "init"
                | "internal"
                | "internals"
                | "wrapper"
                | "runtime"
                | "deps"
                | "dependency"
                | "dependencies"
                | "helper"
                | "helpers"
                | "util"
                | "utils"
        )
    }) {
        return false;
    }
    tokens
        .iter()
        .any(|token| token.len() >= 4 && !is_build_path_segment(token.as_str()))
}

pub(crate) fn disambiguate_semantic_build_variant_source<'a>(
    sources: &[&'a PackageSource],
) -> Option<&'a PackageSource> {
    if sources.is_empty() {
        return None;
    }
    let source_keys = sources
        .iter()
        .map(|source| semantic_build_variant_key(package_source_relative_path(source).as_str()))
        .collect::<BTreeSet<_>>();
    let export_keys = sources
        .iter()
        .map(|source| semantic_build_variant_key(package_source_export_path(source).as_str()))
        .collect::<BTreeSet<_>>();
    let source_key = source_keys.iter().next()?;
    if source_keys.len() != 1 || source_key.is_empty() || export_keys.len() != 1 {
        return None;
    }

    let best_rank = sources
        .iter()
        .map(|source| package_source_external_import_rank(source))
        .min()?;
    let best = sources
        .iter()
        .copied()
        .filter(|source| package_source_external_import_rank(source) == best_rank)
        .collect::<Vec<_>>();
    (best.len() == 1).then_some(best[0])
}

fn semantic_build_variant_key(path: &str) -> Vec<String> {
    canonical_public_path_segments(path)
}

pub(crate) fn semantic_external_source_score(
    source: &PackageSource,
    hint: &str,
) -> (usize, SemanticExternalSourceProof) {
    let source_score =
        package_source_semantic_hint_score(package_source_relative_path(source).as_str(), hint);
    let export_score =
        package_source_semantic_hint_score(package_source_export_path(source).as_str(), hint);
    if export_score > source_score {
        (export_score, SemanticExternalSourceProof::ExportSurface)
    } else {
        (source_score, SemanticExternalSourceProof::SourcePath)
    }
}

pub(crate) fn semantic_source_only_external_source_score(
    source: &PackageSource,
    export_members: &BTreeSet<String>,
    hint: &str,
) -> (usize, SemanticExternalSourceProof) {
    let (path_score, path_proof) = semantic_external_source_score(source, hint);
    let member_score = if semantic_export_member_hint_source_is_narrow(source, export_members) {
        semantic_export_member_hint_score(export_members, hint)
    } else {
        0
    };
    if member_score > path_score
        || (member_score == path_score
            && member_score > 0
            && semantic_external_source_proof_rank(SemanticExternalSourceProof::ExportMember)
                > semantic_external_source_proof_rank(path_proof))
    {
        (member_score, SemanticExternalSourceProof::ExportMember)
    } else {
        (path_score, path_proof)
    }
}

fn semantic_export_member_hint_source_is_narrow(
    source: &PackageSource,
    export_members: &BTreeSet<String>,
) -> bool {
    if export_members.is_empty() || export_members.len() > 8 {
        return false;
    }
    let relative_path = package_source_relative_path(source);
    let leaf = strip_source_extension(relative_path.as_str())
        .trim_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    !matches!(leaf, "" | "index")
}

fn semantic_export_member_hint_score(export_members: &BTreeSet<String>, hint: &str) -> usize {
    let hint = hint.trim().trim_matches('/');
    if hint.is_empty() {
        return 0;
    }
    let hint_last_segment = hint.rsplit('/').next().unwrap_or(hint);
    let hint_last_normalized = normalize_hint_text(hint_last_segment);
    if hint_last_normalized.len() < 4 {
        return 0;
    }
    let hint_tokens = path_hint_tokens(hint_last_segment);
    export_members
        .iter()
        .filter_map(|member| {
            let member_normalized = normalize_hint_text(member);
            if member_normalized.len() < 4 {
                return None;
            }
            if member_normalized == hint_last_normalized {
                return Some(3);
            }
            let member_tokens = path_hint_tokens(member);
            if hint_tokens.len() >= 2
                && !member_tokens.is_empty()
                && hint_tokens
                    .iter()
                    .all(|token| member_tokens.contains(token))
            {
                return Some(3);
            }
            None
        })
        .max()
        .unwrap_or(0)
}
