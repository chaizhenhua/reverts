use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

fn workspace_metadata() -> serde_json::Value {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_manifest = manifest_dir.join("../../Cargo.toml");
    let output = Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--manifest-path",
        ])
        .arg(workspace_manifest)
        .output()
        .expect("cargo metadata should run");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("metadata should be valid json")
}

fn direct_reverts_dependencies(metadata: &serde_json::Value, package: &str) -> BTreeSet<String> {
    let packages = metadata
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .expect("metadata packages should be an array");
    let package = packages
        .iter()
        .find(|candidate| {
            candidate.get("name").and_then(serde_json::Value::as_str) == Some(package)
        })
        .unwrap_or_else(|| panic!("package {package} should exist in workspace metadata"));
    package
        .get("dependencies")
        .and_then(serde_json::Value::as_array)
        .expect("package dependencies should be an array")
        .iter()
        .filter_map(|dependency| dependency.get("name").and_then(serde_json::Value::as_str))
        .filter(|name| name.starts_with("reverts-"))
        .map(ToString::to_string)
        .collect()
}

fn assert_has_no_dependency(metadata: &serde_json::Value, package: &str, forbidden: &[&str]) {
    let dependencies = direct_reverts_dependencies(metadata, package);
    let violations = forbidden
        .iter()
        .copied()
        .filter(|forbidden| dependencies.contains(*forbidden))
        .collect::<Vec<_>>();
    assert!(
        violations.is_empty(),
        "{package} must not depend on {violations:?}; direct reverts deps: {dependencies:?}"
    );
}

#[test]
fn domain_crates_do_not_depend_on_matcher_or_cli_layers() {
    let metadata = workspace_metadata();

    assert_has_no_dependency(
        &metadata,
        "reverts-package",
        &["reverts-package-matcher", "reverts-cli", "reverts-pipeline"],
    );
    assert_has_no_dependency(&metadata, "reverts-package-matcher", &["reverts-cli"]);
    assert_has_no_dependency(&metadata, "reverts-planner", &["reverts-cli"]);
}

#[test]
fn core_data_crates_keep_narrow_dependencies() {
    let metadata = workspace_metadata();

    assert!(
        direct_reverts_dependencies(&metadata, "reverts-ir").is_empty(),
        "reverts-ir must remain the dependency root"
    );

    let input_dependencies = direct_reverts_dependencies(&metadata, "reverts-input");
    let allowed = BTreeSet::from(["reverts-ir".to_string()]);
    let unexpected = input_dependencies
        .difference(&allowed)
        .cloned()
        .collect::<BTreeSet<_>>();
    assert!(
        unexpected.is_empty(),
        "reverts-input should only depend on data-root crates; got {input_dependencies:?}"
    );
}
