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
            let path = normalized_module_output_path(
                module.id,
                program
                    .semantic_names()
                    .module_path(module.id)
                    .unwrap_or(module.semantic_path.as_str()),
            );
            (module.id, path)
        })
        .collect()
}

fn normalized_module_output_path(module_id: ModuleId, raw_path: &str) -> String {
    let raw_path = raw_path.trim();
    if is_safe_typescript_module_path(raw_path) {
        return raw_path.to_string();
    }
    let slug = output_path_slug(strip_source_extension(raw_path));
    format!("modules/{}-{slug}.ts", module_id.0)
}

fn is_safe_typescript_module_path(path: &str) -> bool {
    if !path.ends_with(".ts") && !path.ends_with(".tsx") {
        return false;
    }
    path.split('/').all(|segment| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    })
}

fn strip_source_extension(path: &str) -> &str {
    for extension in [".tsx", ".ts", ".jsx", ".mjs", ".cjs", ".js"] {
        if let Some(stripped) = path.strip_suffix(extension) {
            return stripped;
        }
    }
    path
}

fn output_path_slug(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut last_was_separator = false;
    for ch in value.chars() {
        let mapped = if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '/') {
            ch
        } else {
            '-'
        };
        if mapped == '-' {
            if last_was_separator {
                continue;
            }
            last_was_separator = true;
        } else {
            last_was_separator = false;
        }
        output.push(mapped);
    }
    let trimmed = output.trim_matches(|ch| matches!(ch, '-' | '/' | '.'));
    if trimmed.is_empty() {
        "module".to_string()
    } else {
        trimmed.to_string()
    }
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
