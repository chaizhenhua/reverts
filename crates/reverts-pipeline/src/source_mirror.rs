use std::path::{Component, Path};

use reverts_input::{InputBundle, SourceFileInput};

use crate::EmittedAsset;

pub(crate) fn collect_source_mirror_assets(input: &InputBundle) -> Vec<EmittedAsset> {
    input
        .source_files
        .iter()
        .filter_map(source_mirror_asset)
        .collect()
}

fn source_mirror_asset(source_file: &SourceFileInput) -> Option<EmittedAsset> {
    let source = source_file.source.as_deref()?;
    Some(EmittedAsset {
        path: source_file_preservation_path(source_file),
        bytes: preserved_source(source_file.path.as_str(), source).into_bytes(),
        executable: false,
    })
}

fn preserved_source(source_file_path: &str, source: &str) -> String {
    let mut preserved = format!("// reverts-preserved-source-import-target: {source_file_path}\n");
    preserved.push_str("// @ts-nocheck\n");
    preserved.push_str(source);
    preserved
}

fn source_file_preservation_path(source_file: &SourceFileInput) -> String {
    let path = mirror_relative_path(Path::new(source_file.path.as_str()), source_file.id);
    normalize_path(Path::new("sources").join(path).as_path())
}

fn mirror_relative_path(path: &Path, source_file_id: u32) -> String {
    let mut parts = Vec::<String>::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => parts.push(prefix.as_os_str().to_string_lossy().into()),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => parts.push("__parent__".to_string()),
            Component::Normal(part) => parts.push(part.to_string_lossy().into()),
        }
    }
    if parts.is_empty() {
        format!("source-file-{source_file_id}")
    } else {
        parts.join("/")
    }
}

fn normalize_path(path: &Path) -> String {
    let mut parts = Vec::<String>::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => parts.push(prefix.as_os_str().to_string_lossy().into()),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                if parts.last().is_some_and(|part| part != "..") {
                    parts.pop();
                } else {
                    parts.push("..".to_string());
                }
            }
            Component::Normal(part) => parts.push(part.to_string_lossy().into()),
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use reverts_input::{InputBundle, InputRows, ProjectInput, SourceFileInput};

    #[test]
    fn mirrors_source_files_under_sources_tree() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "/app/assets/main.mjs",
            Some("export const main = 1;".into()),
        ));
        let input = InputBundle::from_rows(rows).expect("fixture input should be valid");

        let assets = super::collect_source_mirror_assets(&input);

        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].path, "sources/app/assets/main.mjs");
        let source = std::str::from_utf8(&assets[0].bytes).expect("source mirror is utf8");
        assert!(source.contains("reverts-preserved-source-import-target: /app/assets/main.mjs"));
        assert!(source.contains("export const main = 1;"));
    }

    #[test]
    fn skips_sources_without_source_text() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "/app/deferred.js", None));
        let input = InputBundle::from_rows(rows).expect("fixture input should be valid");

        assert!(super::collect_source_mirror_assets(&input).is_empty());
    }

    #[test]
    fn parent_segments_cannot_escape_sources_tree() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "../../outside.js",
            Some("export const outside = 1;".into()),
        ));
        let input = InputBundle::from_rows(rows).expect("fixture input should be valid");

        let assets = super::collect_source_mirror_assets(&input);

        assert_eq!(assets[0].path, "sources/__parent__/__parent__/outside.js");
    }
}
