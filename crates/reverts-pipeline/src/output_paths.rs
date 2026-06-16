//! Output-path helpers.
//!
//! - `module_output_paths` builds the `ModuleId → emitted file path` map
//!   the planner uses to anchor cross-file rewrites. It prefers
//!   `semantic_names().module_path` (the planner's chosen layout) and
//!   falls back to the module's own semantic path when the planner has
//!   nothing to say (rare and indicates a planner gap, not a pipeline
//!   bug).
//! - `relative_asset_specifier` computes a POSIX-style relative path
//!   from an emitted source file to an emitted asset, normalized so it
//!   always begins with `./` or `../`. We use POSIX separators because
//!   the relative specifier ends up inside a JS import string.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use reverts_ir::ModuleId;
use reverts_model::EnrichedProgram;

pub(crate) fn module_output_paths(program: &EnrichedProgram) -> BTreeMap<ModuleId, String> {
    program
        .model()
        .modules()
        .iter()
        .map(|module| {
            let path = program
                .semantic_names()
                .module_path(module.id)
                .unwrap_or(module.semantic_path.as_str())
                .to_string();
            (module.id, path)
        })
        .collect()
}

pub(crate) fn relative_asset_specifier(from_file: &str, to_asset: &str) -> String {
    let from_dir = Path::new(from_file)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let from_components = normal_path_components(from_dir);
    let to_components = normal_path_components(Path::new(to_asset));
    let common = from_components
        .iter()
        .zip(to_components.iter())
        .take_while(|(left, right)| left == right)
        .count();

    let mut parts = Vec::new();
    parts.extend(std::iter::repeat_n(
        "..".to_string(),
        from_components.len() - common,
    ));
    parts.extend(to_components[common..].iter().cloned());
    let relative = parts.join("/");
    if relative.starts_with('.') {
        relative
    } else {
        format!("./{relative}")
    }
}

fn normal_path_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            Component::CurDir => None,
            Component::ParentDir => Some("..".to_string()),
            Component::RootDir | Component::Prefix(_) => None,
        })
        .collect()
}
