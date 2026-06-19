//! Explicit pre-accept transform stage.
//!
//! These transforms run after AST-backed emission and before acceptance audits.
//! They are not post-write repair: each pass has a named, planner-visible
//! purpose and the resulting project is still unaudited until the pipeline runs
//! parse/synthesis checks.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Deref;
use std::path::{Component, Path};

use reverts_emitter::{EmittedFile, EmittedProject};
use reverts_input::{InputBundle, SourceFileInput};
use reverts_ir::ModuleId;
use reverts_js::{ParseGoal, collect_static_module_specifiers};
use reverts_observe::AuditReport;

use crate::AssetReference;
use crate::assets::rewrite_emitted_asset_references;
use crate::source_rewrites::{
    canonicalize_emitted_source_locations, fold_multiline_static_template_literals,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreAcceptProject {
    pub project: EmittedProject,
    pub report: PreAcceptTransformReport,
}

impl Deref for PreAcceptProject {
    type Target = EmittedProject;

    fn deref(&self) -> &Self::Target {
        &self.project
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreAcceptTransformReport {
    pub transforms: Vec<PreAcceptTransformEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreAcceptTransformEntry {
    pub name: &'static str,
    pub changed_files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedProject {
    pub project: EmittedProject,
}

impl Deref for AcceptedProject {
    type Target = EmittedProject;

    fn deref(&self) -> &Self::Target {
        &self.project
    }
}

impl PreAcceptProject {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            project: EmittedProject::default(),
            report: PreAcceptTransformReport {
                transforms: Vec::new(),
            },
        }
    }

    #[must_use]
    pub fn accept_if_clean(self, audit: &AuditReport) -> Option<AcceptedProject> {
        (!audit.has_errors()).then_some(AcceptedProject {
            project: self.project,
        })
    }
}

pub(crate) struct PreAcceptContext<'a> {
    pub(crate) input: &'a InputBundle,
    pub(crate) asset_references: &'a [AssetReference],
    pub(crate) module_output_paths: &'a BTreeMap<ModuleId, String>,
}

trait PreAcceptTransform {
    fn name(&self) -> &'static str;
    fn apply(&self, project: &mut EmittedProject, context: &PreAcceptContext<'_>);
}

pub(crate) fn apply_pre_accept_transforms(
    mut project: EmittedProject,
    context: &PreAcceptContext<'_>,
) -> PreAcceptProject {
    let passes: [&dyn PreAcceptTransform; 4] = [
        &CanonicalizeSourceLocations,
        &MaterializeRelativeSourceImports,
        &RewriteAssetReferences,
        &FoldStaticTemplateLiterals,
    ];
    let mut transforms = Vec::with_capacity(passes.len());
    for pass in passes {
        let before = project_fingerprints(&project);
        pass.apply(&mut project, context);
        let after = project_fingerprints(&project);
        let changed_files = before
            .iter()
            .zip(after.iter())
            .filter(
                |((before_path, before_source), (after_path, after_source))| {
                    before_path != after_path || before_source != after_source
                },
            )
            .count()
            + before.len().abs_diff(after.len());
        transforms.push(PreAcceptTransformEntry {
            name: pass.name(),
            changed_files,
        });
    }
    PreAcceptProject {
        project,
        report: PreAcceptTransformReport { transforms },
    }
}

fn project_fingerprints(project: &EmittedProject) -> Vec<(String, String)> {
    project
        .files
        .iter()
        .map(|file| (file.path.clone(), file.source.clone()))
        .collect()
}

struct CanonicalizeSourceLocations;

impl PreAcceptTransform for CanonicalizeSourceLocations {
    fn name(&self) -> &'static str {
        "canonicalize_source_locations"
    }

    fn apply(&self, project: &mut EmittedProject, _context: &PreAcceptContext<'_>) {
        canonicalize_emitted_source_locations(project);
    }
}

struct RewriteAssetReferences;

impl PreAcceptTransform for RewriteAssetReferences {
    fn name(&self) -> &'static str {
        "rewrite_asset_references"
    }

    fn apply(&self, project: &mut EmittedProject, context: &PreAcceptContext<'_>) {
        rewrite_emitted_asset_references(
            project,
            context.input,
            context.asset_references,
            context.module_output_paths,
        );
    }
}

struct MaterializeRelativeSourceImports;

impl PreAcceptTransform for MaterializeRelativeSourceImports {
    fn name(&self) -> &'static str {
        "materialize_relative_source_imports"
    }

    fn apply(&self, project: &mut EmittedProject, context: &PreAcceptContext<'_>) {
        materialize_relative_source_import_targets(project, context);
    }
}

struct FoldStaticTemplateLiterals;

impl PreAcceptTransform for FoldStaticTemplateLiterals {
    fn name(&self) -> &'static str {
        "fold_static_template_literals"
    }

    fn apply(&self, project: &mut EmittedProject, _context: &PreAcceptContext<'_>) {
        fold_multiline_static_template_literals(project);
    }
}

fn preserved_source(source_file_path: &str, source: &str) -> String {
    let mut preserved = format!("// reverts-preserved-source-import-target: {source_file_path}\n");
    preserved.push_str("// @ts-nocheck\n");
    preserved.push_str(source);
    preserved
}

fn materialize_relative_source_import_targets(
    project: &mut EmittedProject,
    context: &PreAcceptContext<'_>,
) {
    let source_files_by_path = context
        .input
        .source_files
        .iter()
        .map(|source_file| {
            (
                normalize_path(Path::new(source_file.path.as_str())),
                source_file,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let source_paths_by_id = context
        .input
        .source_files
        .iter()
        .map(|source_file| (source_file.id, source_file.path.as_str()))
        .collect::<BTreeMap<_, _>>();
    let mut source_origin_by_output_path = BTreeMap::<String, String>::new();
    for module in &context.input.modules {
        let Some(output_path) = context.module_output_paths.get(&module.id) else {
            continue;
        };
        let Some(source_file_id) = module.source_file_id else {
            continue;
        };
        let Some(source_path) = source_paths_by_id.get(&source_file_id) else {
            continue;
        };
        source_origin_by_output_path.insert(output_path.clone(), (*source_path).to_string());
    }

    let mut emitted_paths = project
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect::<BTreeSet<_>>();
    let mut cursor = 0usize;
    while cursor < project.files.len() {
        let file = project.files[cursor].clone();
        cursor += 1;

        let Some(origin_path) = source_origin_by_output_path
            .get(file.path.as_str())
            .cloned()
        else {
            continue;
        };
        let Ok(specifiers) = collect_static_module_specifiers(
            file.source.as_str(),
            Some(Path::new(file.path.as_str())),
            parse_goal_for_path(file.path.as_str()),
        ) else {
            continue;
        };
        for specifier in specifiers {
            let specifier = strip_query_and_fragment(specifier.value.as_str());
            if !specifier.starts_with('.') {
                continue;
            }
            let Some(source_file) = resolve_source_relative_import(
                origin_path.as_str(),
                specifier,
                &source_files_by_path,
            ) else {
                continue;
            };
            let Some(source) = source_file.source.as_deref() else {
                continue;
            };
            let Some(output_path) = emitted_relative_import_target_path(
                file.path.as_str(),
                specifier,
                source_file.path.as_str(),
            ) else {
                continue;
            };
            if emitted_paths.contains(output_path.as_str()) {
                continue;
            }
            project.files.push(EmittedFile {
                path: output_path.clone(),
                source: preserved_source(source_file.path.as_str(), source),
            });
            emitted_paths.insert(output_path.clone());
            source_origin_by_output_path.insert(output_path, source_file.path.clone());
        }
    }
    project
        .files
        .sort_by(|left, right| left.path.cmp(&right.path));
}

fn resolve_source_relative_import<'a>(
    from_source_file: &str,
    specifier: &str,
    source_files_by_path: &BTreeMap<String, &'a SourceFileInput>,
) -> Option<&'a SourceFileInput> {
    let base = normalize_join(
        Path::new(from_source_file)
            .parent()
            .unwrap_or_else(|| Path::new("")),
        specifier,
    );
    source_import_candidates(base.as_str())
        .into_iter()
        .find_map(|candidate| source_files_by_path.get(candidate.as_str()).copied())
}

fn emitted_relative_import_target_path(
    from_output_file: &str,
    specifier: &str,
    source_file_path: &str,
) -> Option<String> {
    let base = normalize_join(
        Path::new(from_output_file)
            .parent()
            .unwrap_or_else(|| Path::new("")),
        specifier,
    );
    if base.starts_with("../") || base == ".." {
        return None;
    }
    Some(typescript_path_for_source_extension(
        base.as_str(),
        Path::new(source_file_path)
            .extension()
            .and_then(std::ffi::OsStr::to_str),
    ))
}

fn source_import_candidates(base: &str) -> Vec<String> {
    let mut candidates = vec![base.to_string()];
    if Path::new(base).extension().is_none() {
        candidates.extend(
            [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"].map(|ext| format!("{base}{ext}")),
        );
        candidates.extend(
            ["index.ts", "index.tsx", "index.js", "index.jsx"].map(|name| format!("{base}/{name}")),
        );
    }
    candidates
}

fn typescript_path_for_source_extension(path: &str, source_extension: Option<&str>) -> String {
    let target_extension = match source_extension {
        Some("jsx") | Some("tsx") => "tsx",
        Some("mjs") | Some("mts") => "mts",
        Some("cjs") | Some("cts") => "cts",
        _ => "ts",
    };
    replace_extension(path, target_extension)
}

fn replace_extension(path: &str, extension: &str) -> String {
    let path = Path::new(path);
    let mut replaced = path.to_path_buf();
    replaced.set_extension(extension);
    path_to_slash_string(replaced.as_path())
}

fn normalize_join(base: &Path, specifier: &str) -> String {
    normalize_path(base.join(specifier).as_path())
}

fn normalize_path(path: &Path) -> String {
    let mut parts = Vec::<String>::new();
    let mut absolute = false;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => parts.push(prefix.as_os_str().to_string_lossy().into()),
            Component::RootDir => absolute = true,
            Component::CurDir => {}
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
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

fn path_to_slash_string(path: &Path) -> String {
    normalize_path(path)
}

fn parse_goal_for_path(path: &str) -> ParseGoal {
    match Path::new(path)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
    {
        Some("js" | "jsx" | "mjs" | "cjs") => ParseGoal::JavaScript,
        _ => ParseGoal::TypeScript,
    }
}

fn strip_query_and_fragment(value: &str) -> &str {
    let query_index = value.find('?').unwrap_or(value.len());
    let fragment_index = value.find('#').unwrap_or(value.len());
    &value[..query_index.min(fragment_index)]
}

#[cfg(test)]
mod tests {
    use super::{
        CanonicalizeSourceLocations, FoldStaticTemplateLiterals, MaterializeRelativeSourceImports,
        PreAcceptContext, PreAcceptTransform, RewriteAssetReferences,
    };
    use reverts_emitter::{EmittedFile, EmittedProject};
    use reverts_input::{InputBundle, InputRows, ModuleInput, ProjectInput, SourceFileInput};
    use reverts_ir::ModuleId;
    use std::collections::BTreeMap;

    #[test]
    fn pre_accept_transform_order_is_explicit() {
        let passes: [&dyn PreAcceptTransform; 4] = [
            &CanonicalizeSourceLocations,
            &MaterializeRelativeSourceImports,
            &RewriteAssetReferences,
            &FoldStaticTemplateLiterals,
        ];
        let names = passes.iter().map(|pass| pass.name()).collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "canonicalize_source_locations",
                "materialize_relative_source_imports",
                "rewrite_asset_references",
                "fold_static_template_literals",
            ]
        );
    }

    #[test]
    fn pre_accept_report_records_named_transform_entries() {
        let report = super::PreAcceptTransformReport {
            transforms: vec![super::PreAcceptTransformEntry {
                name: "example",
                changed_files: 2,
            }],
        };
        assert_eq!(report.transforms[0].name, "example");
        assert_eq!(report.transforms[0].changed_files, 2);
    }

    #[test]
    fn materializes_relative_source_import_targets_at_emitted_relative_path() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "/app/assets/main.js",
            Some("import { helper } from './helper.js'; export const value = helper;".into()),
        ));
        rows.source_files.push(SourceFileInput::new(
            2,
            "/app/assets/helper.js",
            Some("export const helper = 1;".into()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "main", "modules/1-app/assets/main.ts")
                .with_source_file(1),
        );
        let input = InputBundle::from_rows(rows).expect("fixture input should be valid");
        let mut project = EmittedProject {
            files: vec![EmittedFile {
                path: "modules/1-app/assets/main.ts".into(),
                source: "import { helper } from './helper.js'; export const value = helper;".into(),
            }],
        };
        let module_output_paths =
            BTreeMap::from([(ModuleId(1), "modules/1-app/assets/main.ts".to_string())]);
        let context = PreAcceptContext {
            input: &input,
            asset_references: &[],
            module_output_paths: &module_output_paths,
        };

        MaterializeRelativeSourceImports.apply(&mut project, &context);

        let helper = project
            .files
            .iter()
            .find(|file| file.path == "modules/1-app/assets/helper.ts")
            .expect("relative helper should be materialized");
        assert!(helper.source.contains("export const helper = 1;"));
    }
}
