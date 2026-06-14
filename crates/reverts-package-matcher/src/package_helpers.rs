//! Shared package-matching helper utilities used by the matcher and CLI.
//!
//! Keeping these functions in one owner crate prevents the CLI orchestration
//! layer from carrying a second copy of package/path/ownership semantics.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use reverts_input::{
    InputRows, ModuleDependencyTarget, PackageAttributionStatus, PackageEmissionMode,
};
use reverts_ir::ModuleId;
use semver::Version;

use crate::VersionedPackageMatchReport;

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
        attribution.module_id == module_id
            && attribution.status == PackageAttributionStatus::Accepted
            && attribution.emission_mode == PackageEmissionMode::ExternalImport
    })
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
        if attribution.status == PackageAttributionStatus::Accepted
            && attribution.emission_mode == PackageEmissionMode::ExternalImport
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
    if let Some(unscoped) = package_name.strip_prefix('@') {
        prefixes.push(unscoped.to_string());
        prefixes.push(unscoped.replace('/', "-"));
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
}
