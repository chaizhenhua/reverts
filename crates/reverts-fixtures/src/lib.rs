use reverts_input::{InputBundle, InputBundleError, InputRows, ModuleInput, ProjectInput};
use reverts_ir::{ModuleId, ModuleKind};

#[must_use]
pub fn project(name: impl Into<String>) -> ProjectInput {
    ProjectInput {
        id: 1,
        name: name.into(),
    }
}

#[must_use]
pub fn application_module(id: u32, semantic_path: impl Into<String>) -> ModuleInput {
    ModuleInput {
        id: ModuleId(id),
        kind: ModuleKind::Application,
        semantic_path: semantic_path.into(),
        source_file_id: None,
    }
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
    use reverts_ir::ModuleId;

    use super::minimal_bundle;

    #[test]
    fn minimal_fixture_builds_valid_input_bundle() {
        let bundle = minimal_bundle().expect("minimal fixture should be valid");

        assert!(bundle.module_ids().contains(&ModuleId(1)));
    }
}
