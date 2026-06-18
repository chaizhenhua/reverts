//! Externalization-hint reads and package-source promotion.

use std::collections::BTreeSet;

use reverts_ir::hash::fnv1a_hex as stable_hash;
use reverts_package_matcher::{
    PackageSource, package_source_entry_path, package_source_exported_members,
    package_source_normalized_hash,
};
use rusqlite::Connection;

use crate::errors::MatchPackagesError;
use crate::persistence::externalization_hints::{
    PACKAGE_EXTERNALIZATION_HINT_POLICY_VERSION, hint_export_specifier_matches_package,
    load_package_externalization_hints,
};
pub(crate) fn promote_package_sources_with_externalization_hints(
    connection: &Connection,
    package_names: &BTreeSet<String>,
    package_sources: &mut Vec<PackageSource>,
) -> Result<usize, MatchPackagesError> {
    let hints = load_package_externalization_hints(connection, package_names)?;
    if hints.is_empty() || package_sources.is_empty() {
        return Ok(0);
    }
    let mut promoted = Vec::new();
    for hint in hints {
        if hint
            .proof_policy_version
            .is_some_and(|version| version != PACKAGE_EXTERNALIZATION_HINT_POLICY_VERSION)
        {
            continue;
        }
        if !hint_export_specifier_matches_package(
            hint.package_name.as_str(),
            hint.export_specifier.as_str(),
        ) {
            continue;
        }
        if hint.content_hash.is_none()
            && hint.normalized_source_hash.is_none()
            && hint.public_members.is_empty()
        {
            continue;
        }
        let Some(source) = package_sources.iter().find(|source| {
            source.package_name == hint.package_name
                && source.package_version == hint.package_version
                && package_source_entry_path(source) == hint.entry_path
        }) else {
            continue;
        };
        if source.external_importable {
            continue;
        }
        let direct_specifier_match = source.export_specifier == hint.export_specifier;
        if !direct_specifier_match && hint.public_members.is_empty() {
            continue;
        }
        if let Some(content_hash) = hint.content_hash.as_deref()
            && stable_hash(source.source.as_bytes()) != content_hash
        {
            continue;
        }
        if let Some(normalized_source_hash) = hint.normalized_source_hash.as_deref()
            && package_source_normalized_hash(source.source_path.as_str(), source.source.as_str())
                .as_deref()
                != Some(normalized_source_hash)
        {
            continue;
        }
        if !hint.public_members.is_empty() {
            let exported_members = package_source_exported_members(
                source.source_path.as_str(),
                source.source.as_str(),
            );
            if !hint.public_members.is_subset(&exported_members) {
                continue;
            }
        }
        promoted.push(PackageSource::external(
            source.package_name.as_str(),
            source.package_version.as_str(),
            hint.export_specifier.as_str(),
            source.source_path.as_str(),
            source.source.as_str(),
        ));
    }
    let promoted_len = promoted.len();
    package_sources.extend(promoted);
    Ok(promoted_len)
}
