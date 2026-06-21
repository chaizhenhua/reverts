//! Shared package-matching helper utilities used by the matcher and CLI.
//!
//! Keeping these functions in one owner crate prevents the CLI orchestration
//! layer from carrying a second copy of package/path/ownership semantics.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use reverts_input::{InputRows, ModuleDependencyTarget};
use reverts_ir::{ModuleId, split_bare_specifier};
use reverts_package::{
    is_accepted_external_attribution, package_source_entry_path_from_source_path,
};
use semver::Version;

use crate::{PackageSource, VersionedPackageMatchReport};

/// Connected components of an undirected module adjacency, seeded by `seeds`
/// (the seed iteration order determines component order). A seed with no entry in
/// `adjacency` — or an empty one — forms a singleton component. Shared by the
/// dependency-closure ownership strategy and the matcher pipeline, which both
/// previously carried an identical hand-rolled DFS.
pub(crate) fn connected_components(
    adjacency: &BTreeMap<ModuleId, BTreeSet<ModuleId>>,
    seeds: impl IntoIterator<Item = ModuleId>,
) -> Vec<BTreeSet<ModuleId>> {
    let mut seen = BTreeSet::<ModuleId>::new();
    let mut components = Vec::new();
    for seed in seeds {
        if !seen.insert(seed) {
            continue;
        }
        let mut component = BTreeSet::new();
        let mut stack = vec![seed];
        while let Some(current) = stack.pop() {
            component.insert(current);
            if let Some(neighbors) = adjacency.get(&current) {
                for &neighbor in neighbors {
                    if seen.insert(neighbor) {
                        stack.push(neighbor);
                    }
                }
            }
        }
        components.push(component);
    }
    components
}

#[must_use]
pub fn direct_module_dependencies(rows: &InputRows, module_id: ModuleId) -> Vec<ModuleId> {
    rows.dependencies
        .iter()
        .filter(|dependency| dependency.from_module_id == module_id)
        .filter_map(|dependency| match dependency.target {
            ModuleDependencyTarget::Module(target) => Some(target),
            ModuleDependencyTarget::Package { .. } => None,
        })
        .collect()
}

#[must_use]
pub fn direct_module_dependents(rows: &InputRows, module_id: ModuleId) -> Vec<ModuleId> {
    rows.dependencies
        .iter()
        .filter_map(|dependency| match dependency.target {
            ModuleDependencyTarget::Module(target) if target == module_id => {
                Some(dependency.from_module_id)
            }
            ModuleDependencyTarget::Module(_) | ModuleDependencyTarget::Package { .. } => None,
        })
        .collect()
}

#[must_use]
pub fn has_accepted_external_attribution(rows: &InputRows, module_id: ModuleId) -> bool {
    rows.package_attributions.iter().any(|attribution| {
        attribution.module_id == module_id && is_accepted_external_attribution(attribution)
    })
}

#[must_use]
pub fn accepted_external_modules(
    rows: &InputRows,
    report: &VersionedPackageMatchReport,
) -> BTreeSet<ModuleId> {
    report
        .attributions
        .iter()
        .chain(rows.package_attributions.iter())
        .filter(|attribution| is_accepted_external_attribution(attribution))
        .map(|attribution| attribution.module_id)
        .collect()
}

#[must_use]
pub fn ownership_by_module(
    rows: &InputRows,
    report: &VersionedPackageMatchReport,
) -> BTreeMap<ModuleId, (String, String)> {
    let mut ownership_by_module = report
        .matches
        .iter()
        .map(|package_match| {
            (
                package_match.module_id,
                (
                    package_match.package_name.clone(),
                    package_match.package_version.clone(),
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    for attribution in rows
        .package_attributions
        .iter()
        .chain(report.attributions.iter())
    {
        if is_accepted_external_attribution(attribution)
            && let Some(package_version) = attribution.package_version.as_deref()
        {
            ownership_by_module.insert(
                attribution.module_id,
                (
                    attribution.package_name.clone(),
                    package_version.to_string(),
                ),
            );
        }
    }
    ownership_by_module
}

#[must_use]
pub fn is_exact_package_version_hint(version: &str) -> bool {
    Version::parse(version).is_ok()
}

#[must_use]
pub fn is_json_source_path(source_path: &str) -> bool {
    let source_path = source_path
        .split(['?', '#'])
        .next()
        .unwrap_or(source_path)
        .trim();
    matches!(
        Path::new(source_path).extension().and_then(|ext| ext.to_str()),
        Some(ext) if ext.eq_ignore_ascii_case("json")
    )
}

#[must_use]
pub fn package_semantic_path_prefixes(package_name: &str) -> Vec<String> {
    let mut prefixes = vec![package_name.to_string()];
    let normalized_package = normalize_hint_text(package_name);
    if normalized_package.len() >= 4 {
        prefixes.push(normalized_package);
    }
    if let Some(unscoped) = package_name.strip_prefix('@') {
        prefixes.push(unscoped.to_string());
        prefixes.push(unscoped.replace('/', "-"));
        if let Some((_scope, leaf)) = unscoped.split_once('/') {
            let leaf = leaf.trim();
            if leaf.contains('-') || leaf.len() >= 6 {
                prefixes.push(leaf.to_string());
                let normalized_leaf = normalize_hint_text(leaf);
                if normalized_leaf.len() >= 4 {
                    prefixes.push(normalized_leaf);
                }
            }
        }
    }
    prefixes.sort();
    prefixes.dedup();
    prefixes
}

#[must_use]
pub fn strip_source_extension(path: &str) -> &str {
    for extension in [".js", ".mjs", ".cjs", ".ts", ".tsx"] {
        if let Some(stripped) = path.strip_suffix(extension) {
            return stripped;
        }
    }
    path
}

#[must_use]
pub fn path_hint_tokens(value: &str) -> BTreeSet<String> {
    let mut tokens = BTreeSet::new();
    let mut current = String::new();
    let mut previous_lowercase = false;
    for ch in value.chars() {
        if !ch.is_ascii_alphanumeric() {
            if current.len() >= 2 {
                tokens.insert(current.clone());
            }
            current.clear();
            previous_lowercase = false;
            continue;
        }
        if ch.is_ascii_uppercase() && previous_lowercase && !current.is_empty() {
            if current.len() >= 2 {
                tokens.insert(current.clone());
            }
            current.clear();
        }
        current.push(ch.to_ascii_lowercase());
        previous_lowercase = ch.is_ascii_lowercase();
    }
    if current.len() >= 2 {
        tokens.insert(current);
    }
    for noise in ["js", "ts", "mjs", "cjs", "index"] {
        tokens.remove(noise);
    }
    tokens
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticPathHintMode {
    /// Strict hints used to prove an import surface.
    ImportProof,
    /// Relaxed, still structured hints used only when the caller already has
    /// package ownership evidence and also requires a high-confidence export
    /// surface match. Unlike [`ImportProof`], this may trust a structured
    /// module semantic path even when the minified source body no longer
    /// contains the path token.
    RelaxedImportProof,
    /// Broader hints used only after ownership has already been accepted and
    /// the pipeline must pick a forced external import target.
    ForcedExternal,
}

#[must_use]
pub fn module_package_semantic_path_hints(
    package_name: &str,
    semantic_path: &str,
    module_source: &str,
    mode: SemanticPathHintMode,
) -> Vec<String> {
    let clean = semantic_path
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/')
        .replace('\\', "/");
    if clean.is_empty() {
        return Vec::new();
    }
    let mut prefixes = package_semantic_path_prefixes(package_name);
    if mode == SemanticPathHintMode::ForcedExternal {
        let normalized_package = normalize_hint_text(package_name);
        if !normalized_package.is_empty() {
            prefixes.push(normalized_package);
        }
        prefixes.sort();
        prefixes.dedup();
    }
    let mut hints = Vec::new();
    for prefix in prefixes {
        let Some(rest) = strip_package_prefix_from_semantic_path(clean.as_str(), prefix.as_str())
        else {
            continue;
        };
        let hint = strip_source_extension(rest)
            .trim_matches('/')
            .to_ascii_lowercase();
        if !hint.is_empty()
            && (mode == SemanticPathHintMode::ForcedExternal
                || module_source.trim().is_empty()
                || source_contains_semantic_hint(module_source, hint.as_str())
                || hint
                    .rsplit('/')
                    .next()
                    .is_some_and(|segment| source_contains_semantic_hint(module_source, segment))
                || (mode == SemanticPathHintMode::RelaxedImportProof
                    && relaxed_semantic_hint_is_import_proof(hint.as_str()))
                || semantic_filename_hint_is_package_root(package_name, hint.as_str()))
        {
            hints.push(hint);
        }
    }
    let clean_hint = strip_source_extension(clean.as_str())
        .trim_matches('/')
        .to_ascii_lowercase();
    if matches!(
        mode,
        SemanticPathHintMode::ImportProof | SemanticPathHintMode::RelaxedImportProof
    ) && !clean_hint.is_empty()
        && !clean_hint.starts_with("modules/")
        && !package_semantic_path_prefixes(package_name)
            .iter()
            .any(|prefix| {
                clean_hint == *prefix || clean_hint.starts_with(format!("{prefix}/").as_str())
            })
        && (semantic_filename_hint_is_structured_export_path(clean_hint.as_str())
            || semantic_filename_hint_is_package_root(package_name, clean_hint.as_str()))
    {
        hints.push(clean_hint);
    }
    if let Some(hint) = module_semantic_filename_hint(clean.as_str(), module_source)
        && (mode == SemanticPathHintMode::ForcedExternal
            || semantic_filename_hint_is_package_export_like(hint.as_str())
            || semantic_filename_hint_is_structured_export_path(hint.as_str())
            || semantic_filename_hint_is_package_root(package_name, hint.as_str()))
    {
        hints.push(hint);
    }
    hints.sort();
    hints.dedup();
    hints
}

#[must_use]
pub fn clean_package_semantic_path_hint(package_name: &str, semantic_path: &str) -> Option<String> {
    module_package_semantic_path_hints(
        package_name,
        semantic_path,
        "",
        SemanticPathHintMode::ImportProof,
    )
    .into_iter()
    .find(|hint| is_useful_package_path_hint(hint.as_str()))
}

fn module_semantic_filename_hint(semantic_path: &str, module_source: &str) -> Option<String> {
    let filename = semantic_path.rsplit('/').next().unwrap_or(semantic_path);
    let generated_stem = semantic_path
        .strip_prefix("modules/")
        .map(strip_source_extension)
        .map(str::trim);
    let stem = generated_stem.unwrap_or_else(|| strip_source_extension(filename).trim());
    let (prefix, rest) = stem.split_once('-')?;
    if prefix.is_empty() || !prefix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let hint = rest.trim_matches('/').to_ascii_lowercase();
    let source_contains_hint = source_contains_semantic_hint(module_source, hint.as_str());
    let structured_public_export_hint =
        semantic_filename_hint_is_structured_export_path(hint.as_str())
            && !hint.split('/').any(|segment| segment == "_internal");
    if hint.is_empty() || (!source_contains_hint && !structured_public_export_hint) {
        return None;
    }
    Some(hint)
}

fn semantic_filename_hint_is_package_export_like(hint: &str) -> bool {
    let trimmed = hint.trim();
    trimmed.starts_with('_')
        && trimmed
            .chars()
            .any(|character| character.is_ascii_alphabetic() && character.is_ascii_lowercase())
}

fn relaxed_semantic_hint_is_import_proof(hint: &str) -> bool {
    let trimmed = hint.trim().trim_matches('/');
    if trimmed.is_empty()
        || trimmed.split('/').any(|segment| {
            matches!(
                segment,
                "_init" | "init" | "init-wrapper" | "_internal" | "internal" | "internals"
            )
        })
    {
        return false;
    }
    semantic_filename_hint_is_structured_export_path(trimmed)
        || (is_useful_package_path_hint(trimmed)
            && !is_build_path_segment(normalize_hint_text(trimmed).as_str()))
}

fn semantic_filename_hint_is_structured_export_path(hint: &str) -> bool {
    let trimmed = hint.trim().trim_matches('/');
    let canonical_segments = canonical_public_path_segments(trimmed);
    let raw_segments = public_path_segments_without_build_stripping(trimmed);
    [canonical_segments, raw_segments]
        .into_iter()
        .any(|segments| {
            segments.len() >= 2
                && !segments
                    .iter()
                    .all(|segment| is_build_path_segment(segment.as_str()))
                && segments
                    .last()
                    .is_some_and(|segment| normalize_hint_text(segment).len() >= 4)
        })
}

fn semantic_filename_hint_is_package_root(package_name: &str, hint: &str) -> bool {
    let hint = normalize_hint_text(hint);
    package_semantic_path_prefixes(package_name)
        .into_iter()
        .any(|prefix| normalize_hint_text(prefix.as_str()) == hint)
}

fn source_contains_semantic_hint(source: &str, hint: &str) -> bool {
    let source_normalized = normalize_hint_text(source);
    let hint_normalized = normalize_hint_text(hint);
    hint_normalized.len() >= 4 && source_normalized.contains(hint_normalized.as_str())
}

#[must_use]
pub fn package_source_relative_path(source: &PackageSource) -> String {
    package_source_entry_path(source).to_ascii_lowercase()
}

#[must_use]
pub fn package_source_entry_path(source: &PackageSource) -> String {
    package_source_entry_path_from_source_path(
        source.package_name.as_str(),
        source.package_version.as_str(),
        source.source_path.as_str(),
    )
}

#[must_use]
pub fn package_source_external_import_rank(source: &PackageSource) -> u8 {
    let path = package_source_entry_path(source).to_ascii_lowercase();
    let extension = Path::new(path.as_str())
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    if extension == "mjs"
        || path_has_any_segment(
            path.as_str(),
            &["dist-es", "esm", "es", "module", "modules"],
        )
    {
        return 0;
    }
    if extension == "cjs" || path_has_any_segment(path.as_str(), &["dist-cjs", "cjs", "commonjs"]) {
        return 1;
    }
    if path_has_any_segment(path.as_str(), &["node"]) {
        return 2;
    }
    if path_has_any_segment(path.as_str(), &["browser"]) {
        return 3;
    }
    if path_has_any_segment(path.as_str(), &["umd", "bundles", "bundle"]) {
        return 4;
    }
    if path_has_any_segment(path.as_str(), &["dist", "lib", "build"]) {
        return 5;
    }
    let path_segments = path
        .split('/')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if path_segments.len() == 1
        && matches!(
            path_segments.first().copied(),
            Some("index.js" | "index.mjs")
        )
    {
        return 6;
    }
    if path_segments.len() == 1 {
        return 7;
    }
    if path_has_any_segment(path.as_str(), &["src", "source", "sources"]) {
        return 8;
    }
    9
}

fn path_has_any_segment(path: &str, candidates: &[&str]) -> bool {
    path.split('/')
        .map(|segment| segment.trim())
        .any(|segment| candidates.contains(&segment))
}

#[must_use]
pub fn package_source_export_path(source: &PackageSource) -> String {
    let specifier = source.export_specifier.trim();
    match split_bare_specifier(specifier) {
        Some((package_name, None)) if package_name == source.package_name => String::new(),
        Some((package_name, Some(subpath))) if package_name == source.package_name => {
            subpath.to_ascii_lowercase()
        }
        _ => specifier
            .trim_start_matches("./")
            .trim_start_matches('/')
            .to_ascii_lowercase(),
    }
}

#[must_use]
pub fn package_source_semantic_surface_hint_score(source: &PackageSource, hint: &str) -> usize {
    package_source_semantic_hint_score(package_source_relative_path(source).as_str(), hint).max(
        package_source_semantic_hint_score(package_source_export_path(source).as_str(), hint),
    )
}

#[must_use]
pub fn package_source_semantic_hint_score(source_path: &str, hint: &str) -> usize {
    let source_segments = canonical_public_path_segments(source_path);
    let hint_segments = canonical_public_path_segments(hint);
    if !hint_segments.is_empty() && source_segments == hint_segments {
        return 5;
    }
    if semantic_path_segments_are_root_like(&source_segments)
        && semantic_path_segments_are_root_like(&hint_segments)
    {
        return 5;
    }
    if !hint_segments.is_empty()
        && path_segments_end_with(&source_segments, &hint_segments)
        && hint_segments.len() >= 2
    {
        return 4;
    }

    let hint_last_segment = hint.rsplit('/').next().unwrap_or(hint);
    let hint_last_normalized = normalize_hint_text(hint_last_segment);
    if hint_last_normalized.len() >= 4
        && source_segments
            .last()
            .is_some_and(|segment| normalize_hint_text(segment) == hint_last_normalized)
    {
        return 3;
    }

    let source_normalized = normalize_hint_text(source_path);
    let hint_normalized = normalize_hint_text(hint);
    if hint_normalized.len() >= 4 && source_normalized.contains(hint_normalized.as_str()) {
        return 3;
    }

    if hint_last_normalized.len() >= 4 && source_normalized.contains(hint_last_normalized.as_str())
    {
        return 2;
    }

    let source_tokens = path_hint_tokens(source_path);
    let hint_tokens = path_hint_tokens(hint_last_segment);
    if !hint_tokens.is_empty()
        && hint_tokens
            .iter()
            .all(|token| source_tokens.contains(token))
    {
        1
    } else {
        0
    }
}

#[must_use]
pub fn strip_package_prefix_from_semantic_path<'a>(
    semantic_path: &'a str,
    prefix: &str,
) -> Option<&'a str> {
    if let Some(rest) = semantic_path.strip_prefix(format!("{prefix}/").as_str()) {
        return Some(rest);
    }
    for marker in [format!("/{prefix}/"), format!("-{prefix}/")] {
        if let Some(index) = semantic_path.find(marker.as_str()) {
            return semantic_path.get(index + marker.len()..);
        }
    }
    None
}

#[must_use]
pub fn canonical_public_path_segments(value: &str) -> Vec<String> {
    let mut segments = public_path_segments_without_build_stripping(value);
    while segments.len() > 1
        && segments
            .first()
            .is_some_and(|segment| is_build_path_segment(segment.as_str()))
    {
        segments.remove(0);
    }
    if segments.len() > 1 && segments.last().is_some_and(|segment| segment == "index") {
        segments.pop();
    }
    segments
}

fn public_path_segments_without_build_stripping(value: &str) -> Vec<String> {
    let clean = value
        .split(['?', '#'])
        .next()
        .unwrap_or(value)
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/')
        .replace('\\', "/");
    let clean = strip_source_extension(clean.as_str()).trim_matches('/');
    clean
        .split('/')
        .map(str::trim)
        .map(normalize_hint_text)
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
}

#[must_use]
pub fn is_build_path_segment(segment: &str) -> bool {
    matches!(
        segment,
        "src"
            | "source"
            | "sources"
            | "dist"
            | "build"
            | "lib"
            | "libs"
            | "esm"
            | "es"
            | "cjs"
            | "commonjs"
            | "module"
            | "modules"
            | "browser"
            | "umd"
    )
}

#[must_use]
pub fn semantic_path_segments_are_root_like(segments: &[String]) -> bool {
    segments.is_empty()
        || (segments.len() == 1 && segments[0] == "index")
        || (segments.last().is_some_and(|segment| segment == "index")
            && segments[..segments.len().saturating_sub(1)]
                .iter()
                .all(|segment| is_build_path_segment(segment.as_str())))
}

#[must_use]
pub fn path_segments_end_with(segments: &[String], suffix: &[String]) -> bool {
    !suffix.is_empty()
        && suffix.len() <= segments.len()
        && segments[segments.len() - suffix.len()..] == suffix[..]
}

fn is_useful_package_path_hint(hint: &str) -> bool {
    if hint.is_empty() {
        return false;
    }
    let last_segment = hint.rsplit('/').next().unwrap_or(hint);
    last_segment
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .count()
        >= 4
}

#[must_use]
pub fn normalize_hint_text(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reverts_input::{
        ModuleDependencyInput, ModuleDependencyTarget, ModuleInput, PackageAttributionInput,
        ProjectInput,
    };

    #[test]
    fn json_source_path_ignores_query_hash_and_case() {
        assert!(is_json_source_path("data/manifest.JSON?raw#x"));
        assert!(is_json_source_path(" package.json "));
        assert!(!is_json_source_path("data/manifest.json.ts"));
    }

    #[test]
    fn package_path_tokens_split_case_and_drop_source_noise() {
        let tokens = path_hint_tokens("dist/index.nodeValue.cjs");
        assert!(tokens.contains("node"));
        assert!(tokens.contains("value"));
        assert!(!tokens.contains("index"));
        assert!(!tokens.contains("cjs"));
    }

    #[test]
    fn ownership_prefers_report_matches_then_accepted_attributions() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(1),
            "pkg",
            "pkg.js",
            "pkg",
            Some("1.2.3".to_string()),
        ));
        rows.package_attributions
            .push(PackageAttributionInput::accepted_external(
                ModuleId(1),
                "pkg",
                "1.2.3",
                "pkg",
            ));
        let report = VersionedPackageMatchReport {
            attributions: Vec::new(),
            surfaces: Vec::new(),
            matches: Vec::new(),
            version_matches: Vec::new(),
            audit: Default::default(),
        };

        let ownership = ownership_by_module(&rows, &report);

        assert_eq!(
            ownership[&ModuleId(1)],
            ("pkg".to_string(), "1.2.3".to_string())
        );
        assert!(has_accepted_external_attribution(&rows, ModuleId(1)));
    }

    #[test]
    fn direct_module_neighbors_ignore_package_targets() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(1),
            target: ModuleDependencyTarget::Module(ModuleId(2)),
        });
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(1),
            target: ModuleDependencyTarget::Package {
                specifier: "pkg".to_string(),
            },
        });

        assert_eq!(
            direct_module_dependencies(&rows, ModuleId(1)),
            vec![ModuleId(2)]
        );
        assert_eq!(
            direct_module_dependents(&rows, ModuleId(2)),
            vec![ModuleId(1)]
        );
    }

    #[test]
    fn semantic_path_hints_have_strict_and_forced_modes() {
        assert_eq!(
            module_package_semantic_path_hints(
                "pkg",
                "modules/10-basekeys.ts",
                "function basekeys() {}",
                SemanticPathHintMode::ImportProof,
            ),
            Vec::<String>::new()
        );
        assert_eq!(
            module_package_semantic_path_hints(
                "pkg",
                "modules/10-basekeys.ts",
                "function basekeys() {}",
                SemanticPathHintMode::ForcedExternal,
            ),
            vec!["basekeys".to_string()]
        );
        assert_eq!(
            module_package_semantic_path_hints(
                "rxjs",
                "modules/10-rxjs/operators/sample.ts",
                "function a(){return 1;}",
                SemanticPathHintMode::ImportProof,
            ),
            vec!["rxjs/operators/sample".to_string()]
        );
        assert_eq!(
            module_package_semantic_path_hints(
                "rxjs",
                "modules/10-rxjs/operators/sample.ts",
                "function a(){return 1;}",
                SemanticPathHintMode::RelaxedImportProof,
            ),
            vec![
                "operators/sample".to_string(),
                "rxjs/operators/sample".to_string(),
            ]
        );
        assert_eq!(
            module_package_semantic_path_hints(
                "color-convert",
                "modules/11-color-convert/conversions.ts",
                "function a(){return 1;}",
                SemanticPathHintMode::RelaxedImportProof,
            ),
            vec![
                "color-convert/conversions".to_string(),
                "conversions".to_string(),
            ]
        );
        assert_eq!(
            module_package_semantic_path_hints(
                "highlight.js",
                "modules/12-highlightjs/languages/perl.ts",
                "function perl() {}",
                SemanticPathHintMode::RelaxedImportProof,
            ),
            vec![
                "highlightjs/languages/perl".to_string(),
                "languages/perl".to_string(),
            ]
        );
        assert_eq!(
            module_package_semantic_path_hints(
                "@azure/msal-common",
                "modules/13-msal-common/request/AuthorizationCodeRequest.ts",
                "function AuthorizationCodeRequest() {}",
                SemanticPathHintMode::RelaxedImportProof,
            ),
            vec![
                "msal-common/request/authorizationcoderequest".to_string(),
                "request/authorizationcoderequest".to_string(),
            ]
        );
        assert_eq!(
            module_package_semantic_path_hints(
                "rxjs",
                "modules/10-rxjs/_internal/is-array-like.ts",
                "function a(){return 1;}",
                SemanticPathHintMode::RelaxedImportProof,
            ),
            Vec::<String>::new(),
            "weak relaxed hints must not externalize private/internal paths without a source anchor"
        );
        assert_eq!(
            module_package_semantic_path_hints(
                "form-data",
                "modules/12-lib/form_data.ts",
                "function a(){return 1;}",
                SemanticPathHintMode::ImportProof,
            ),
            vec!["lib/form_data".to_string()]
        );
        assert_eq!(
            clean_package_semantic_path_hint("pkg", "node_modules/pkg/lib/basekeys.js").as_deref(),
            Some("lib/basekeys")
        );
        assert!(package_source_semantic_hint_score("dist/lib/basekeys.js", "lib/basekeys") > 0);
    }

    #[test]
    fn package_source_surface_hint_score_uses_export_specifier() {
        let source = PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/public/api",
            "pkg@1.2.3/dist/index.js",
            "export const api = 1;",
        );

        assert_eq!(
            package_source_semantic_hint_score(
                package_source_relative_path(&source).as_str(),
                "public/api"
            ),
            0,
            "the concrete build target is too generic to prove the public export"
        );
        assert_eq!(package_source_export_path(&source), "public/api");
        assert_eq!(
            package_source_semantic_surface_hint_score(&source, "public/api"),
            5
        );
    }

    #[test]
    fn package_source_semantic_hint_score_prefers_exact_leaf_over_contains() {
        assert_eq!(
            package_source_semantic_hint_score("lib/languages/python.js", "highlightjs/python"),
            3
        );
        assert_eq!(
            package_source_semantic_hint_score(
                "lib/languages/python-profiler.js",
                "highlightjs/python"
            ),
            2,
            "prefix-containing siblings must rank below the exact leaf"
        );
    }

    #[test]
    fn package_source_external_import_rank_prefers_esm_and_root_index() {
        let esm = PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg",
            "pkg@1.2.3/dist-es/index.js",
            "export {};",
        );
        let cjs = PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg",
            "pkg@1.2.3/dist-cjs/index.js",
            "module.exports = {};",
        );
        let index = PackageSource::external(
            "lodash",
            "4.2.0",
            "lodash",
            "lodash@4.2.0/index.js",
            "module.exports = {};",
        );
        let named_root = PackageSource::external(
            "lodash",
            "4.2.0",
            "lodash",
            "lodash@4.2.0/lodash.js",
            "module.exports = {};",
        );

        assert!(
            package_source_external_import_rank(&esm) < package_source_external_import_rank(&cjs)
        );
        assert!(
            package_source_external_import_rank(&index)
                < package_source_external_import_rank(&named_root)
        );
    }
}
