pub mod external_corpus;

use reverts_input::{InputBundle, InputBundleError, InputRows, ModuleInput, ProjectInput};
use reverts_ir::ModuleId;

#[must_use]
pub fn project(name: impl Into<String>) -> ProjectInput {
    ProjectInput {
        id: 1,
        name: name.into(),
    }
}

#[must_use]
pub fn application_module(id: u32, semantic_path: impl Into<String>) -> ModuleInput {
    ModuleInput::application(ModuleId(id), format!("m{id}"), semantic_path)
}

#[must_use]
pub fn minimal_rows() -> InputRows {
    let mut rows = InputRows::new(project("fixture"));
    rows.modules.push(application_module(1, "src/index.ts"));
    rows
}

pub fn minimal_bundle() -> Result<InputBundle, InputBundleError> {
    InputBundle::from_rows(minimal_rows())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use reverts_ir::ModuleId;

    use super::minimal_bundle;

    #[test]
    fn minimal_fixture_builds_valid_input_bundle() {
        let bundle = minimal_bundle().expect("minimal fixture should be valid");

        assert!(bundle.module_ids().contains(&ModuleId(1)));
    }

    #[test]
    fn retired_output_core_crate_is_not_a_parallel_implementation() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let crates_dir = manifest_dir.parent().expect("fixture crate has parent dir");
        let workspace_dir = crates_dir
            .parent()
            .expect("crates dir has workspace parent");
        let workspace_manifest = fs::read_to_string(workspace_dir.join("Cargo.toml"))
            .expect("workspace manifest should be readable");

        assert!(!crates_dir.join("reverts-output-core").exists());
        assert!(!workspace_manifest.contains("reverts-output-core"));
    }
}
