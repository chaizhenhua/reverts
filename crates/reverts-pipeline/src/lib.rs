use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::path::{Component, Path};

use reverts_analyze::enrich_program;
use reverts_emitter::{EmitError, emit_project};
use reverts_input::{
    InputBundle, InputRows, ModuleInput, PackageAttributionStatus, SourceFileInput,
};
use reverts_ir::{BindingName, BindingShape, ModuleId, ModuleKind};
use reverts_js::{
    DeclarationCallability, ParseGoal, classify_top_level_bindings,
    collect_file_url_source_location_rewrites, collect_path_builder_calls,
    collect_static_resource_specifiers, collect_string_literals, parse_error_message, parse_source,
};
use reverts_model::{EnrichedProgram, ProgramModel};
use reverts_observe::{AuditFinding, AuditReport, FindingCode};
use reverts_planner::{EmitPlan, ImportExportPlanner, PlanError, PlannedFile};

pub use reverts_emitter::{EmittedFile, EmittedProject};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputRun {
    pub project: EmittedProject,
    pub audit: AuditReport,
    pub runtime_dependencies: Vec<RuntimeDependency>,
    pub assets: Vec<EmittedAsset>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeDependency {
    pub package_name: String,
    pub package_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmittedAsset {
    pub path: String,
    pub bytes: Vec<u8>,
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AssetReference {
    pub module_id: ModuleId,
    pub logical_path: String,
}

pub fn generate_project_from_input(input: InputBundle) -> Result<OutputRun, PipelineError> {
    let model = ProgramModel::from_input(input);
    let enrichment = enrich_program(model);
    let mut audit = enrichment.audit;
    let input = enrichment.program.model().input();
    let runtime_dependencies = collect_runtime_dependencies(input);
    let asset_references = collect_required_asset_references(input);
    let assets = collect_emitted_assets(input, &asset_references);
    audit.extend(audit_required_sources(&enrichment.program));
    audit.extend(audit_required_assets(input, &asset_references));
    if !audit.is_clean() {
        return Ok(OutputRun {
            project: EmittedProject::default(),
            audit,
            runtime_dependencies,
            assets,
        });
    }

    let planner = ImportExportPlanner;
    let plan = planner
        .plan_enriched_program(&enrichment.program)
        .map_err(PipelineError::Plan)?;
    audit.extend(audit_emit_plan_synthesis(&plan));
    if !audit.is_clean() {
        return Ok(OutputRun {
            project: EmittedProject::default(),
            audit,
            runtime_dependencies,
            assets,
        });
    }

    let module_output_paths = module_output_paths(&enrichment.program);
    let mut project = emit_project(&plan).map_err(PipelineError::Emit)?;
    canonicalize_emitted_source_locations(&mut project);
    rewrite_emitted_asset_references(&mut project, input, &asset_references, &module_output_paths);

    audit.extend(audit_emitted_project_parse(&project));
    audit.extend(audit_binding_shape_consistency(&plan, &project));

    Ok(OutputRun {
        project,
        audit,
        runtime_dependencies,
        assets,
    })
}

fn collect_runtime_dependencies(input: &InputBundle) -> Vec<RuntimeDependency> {
    let mut dependencies = BTreeMap::<String, String>::new();

    for attribution in &input.package_attributions {
        if attribution.status != PackageAttributionStatus::Accepted
            || !attribution.emission_mode.requires_runtime_dependency()
        {
            continue;
        }
        let Some(package_version) = attribution.package_version.as_deref() else {
            continue;
        };
        dependencies
            .entry(attribution.package_name.clone())
            .or_insert_with(|| package_version.to_string());
    }

    for package_surface in &input.package_surfaces {
        if package_surface.status != PackageAttributionStatus::Accepted {
            continue;
        }
        let Some(package_version) = package_surface.package_version.as_deref() else {
            continue;
        };
        dependencies
            .entry(package_surface.package_name.clone())
            .or_insert_with(|| package_version.to_string());
    }

    dependencies
        .into_iter()
        .map(|(package_name, package_version)| RuntimeDependency {
            package_name,
            package_version,
        })
        .collect()
}

fn collect_emitted_assets(input: &InputBundle, references: &[AssetReference]) -> Vec<EmittedAsset> {
    let required_logical_paths = references
        .iter()
        .map(|reference| reference.logical_path.as_str())
        .collect::<BTreeSet<_>>();
    input
        .assets
        .iter()
        .filter(|asset| required_logical_paths.contains(asset.logical_path.as_str()))
        .map(|asset| EmittedAsset {
            path: asset.output_path.clone(),
            bytes: asset.bytes.clone(),
            executable: asset.executable,
        })
        .collect()
}

#[must_use]
pub fn collect_required_asset_references(input: &InputBundle) -> Vec<AssetReference> {
    collect_required_asset_references_from_parts(&input.modules, &input.source_files, |module_id| {
        input
            .module_source_slice(module_id)
            .map(|slice| (slice.source_file_path.to_string(), slice.source.to_string()))
    })
}

#[must_use]
pub fn collect_required_asset_references_from_rows(rows: &InputRows) -> Vec<AssetReference> {
    collect_required_asset_references_from_parts(&rows.modules, &rows.source_files, |module_id| {
        rows.module_source_slice(module_id)
            .map(|slice| (slice.source_file_path.to_string(), slice.source.to_string()))
    })
}

fn collect_required_asset_references_from_parts(
    modules: &[ModuleInput],
    _source_files: &[SourceFileInput],
    source_for_module: impl Fn(ModuleId) -> Option<(String, String)>,
) -> Vec<AssetReference> {
    let mut references = BTreeSet::new();
    for module in modules {
        if module.kind == ModuleKind::Package {
            continue;
        }
        let Some((source_file_path, source)) = source_for_module(module.id) else {
            continue;
        };
        let Ok(literals) = collect_static_resource_specifiers(
            source.as_str(),
            Some(Path::new(source_file_path.as_str())),
            ParseGoal::TypeScript,
        ) else {
            // No heuristic fallback: parse failures are already surfaced by
            // AstFactExtractionFailed during enrichment.
            continue;
        };
        for literal in literals {
            if is_asset_reference_literal(literal.value.as_str()) {
                references.insert(AssetReference {
                    module_id: module.id,
                    logical_path: literal.value,
                });
            }
        }
        for logical_path in
            collect_dynamic_asset_references(source.as_str(), source_file_path.as_str())
        {
            references.insert(AssetReference {
                module_id: module.id,
                logical_path,
            });
        }
    }
    references.into_iter().collect()
}

fn collect_dynamic_asset_references(source: &str, source_file_path: &str) -> Vec<String> {
    let Ok(path_calls) = collect_path_builder_calls(
        source,
        Some(Path::new(source_file_path)),
        ParseGoal::TypeScript,
    ) else {
        return Vec::new();
    };

    let values = path_calls
        .iter()
        .flat_map(|call| call.string_arguments.iter().map(String::as_str))
        .collect::<BTreeSet<_>>();
    let has_ripgrep_vendor_prefix = path_calls
        .iter()
        .any(|call| contains_adjacent_segments(&call.string_arguments, &["vendor", "ripgrep"]))
        || (values.contains("vendor") && values.contains("ripgrep"));

    if !has_ripgrep_vendor_prefix {
        return Vec::new();
    }

    let mut references = BTreeSet::<String>::new();
    for call in &path_calls {
        for platform_dir in call
            .string_arguments
            .iter()
            .map(String::as_str)
            .filter(|value| is_node_platform_dir(value))
        {
            if call.string_arguments.iter().any(|value| value == "rg") {
                references.insert(format!("vendor/ripgrep/{platform_dir}/rg"));
            }
            if call.string_arguments.iter().any(|value| value == "rg.exe") {
                references.insert(format!("vendor/ripgrep/{platform_dir}/rg.exe"));
            }
        }

        let call_source = source
            .get(call.byte_start as usize..call.byte_end as usize)
            .unwrap_or_default();
        if call.string_arguments.iter().any(|value| value == "rg")
            && call_source.contains("process.arch")
            && call_source.contains("process.platform")
            && let Some(platform_dir) = current_node_platform_dir()
        {
            references.insert(format!("vendor/ripgrep/{platform_dir}/rg"));
        }
    }

    references.into_iter().collect()
}

fn contains_adjacent_segments(arguments: &[String], segments: &[&str]) -> bool {
    if segments.is_empty() || arguments.len() < segments.len() {
        return false;
    }
    arguments.windows(segments.len()).any(|window| {
        window
            .iter()
            .map(String::as_str)
            .eq(segments.iter().copied())
    })
}

fn is_node_platform_dir(value: &str) -> bool {
    let Some((arch, platform)) = value.split_once('-') else {
        return false;
    };
    matches!(arch, "x64" | "arm64" | "arm") && matches!(platform, "linux" | "darwin" | "win32")
}

fn current_node_platform_dir() -> Option<String> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "arm" => "arm",
        _ => return None,
    };
    let platform = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        "windows" => "win32",
        _ => return None,
    };
    Some(format!("{arch}-{platform}"))
}

fn audit_required_assets(input: &InputBundle, references: &[AssetReference]) -> AuditReport {
    let available = input
        .assets
        .iter()
        .map(|asset| asset.logical_path.as_str())
        .collect::<BTreeSet<_>>();
    let mut audit = AuditReport::default();
    for reference in references {
        if available.contains(reference.logical_path.as_str()) {
            continue;
        }
        audit.push(
            AuditFinding::error(
                FindingCode::MissingRequiredAsset,
                "source references an asset that is absent from project_assets",
            )
            .with_module(reference.module_id.0.to_string())
            .with_binding(reference.logical_path.clone()),
        );
    }
    audit
}

fn is_asset_reference_literal(value: &str) -> bool {
    let path = strip_query_and_fragment(value);
    if path.starts_with("/$bunfs/root/") || path.starts_with("bun:/") {
        return true;
    }

    if path.trim() != path || path.is_empty() || path.chars().any(char::is_whitespace) {
        return false;
    }
    let is_relative = path.starts_with("./") || path.starts_with("../");
    let is_absolute = path.starts_with('/');
    let is_vendor_path = path.starts_with("vendor/") || path.contains("/vendor/");
    let lower = path.to_ascii_lowercase();
    let has_asset_extension = matches!(
        Path::new(lower.as_str())
            .extension()
            .and_then(std::ffi::OsStr::to_str),
        Some(
            "wasm"
                | "node"
                | "so"
                | "dylib"
                | "dll"
                | "exe"
                | "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "svg"
                | "webp"
                | "avif"
                | "ico"
                | "ttf"
                | "otf"
                | "woff"
                | "woff2"
                | "css"
                | "html"
        )
    );

    has_asset_extension && (is_relative || is_absolute || is_vendor_path)
}

fn strip_query_and_fragment(value: &str) -> &str {
    let query_index = value.find('?').unwrap_or(value.len());
    let fragment_index = value.find('#').unwrap_or(value.len());
    &value[..query_index.min(fragment_index)]
}

fn rewrite_emitted_asset_references(
    project: &mut EmittedProject,
    input: &InputBundle,
    references: &[AssetReference],
    module_output_paths: &BTreeMap<ModuleId, String>,
) {
    let assets_by_logical_path = input
        .assets
        .iter()
        .map(|asset| (asset.logical_path.as_str(), asset.output_path.as_str()))
        .collect::<BTreeMap<_, _>>();
    let mut rewrites_by_file = BTreeMap::<String, BTreeMap<String, String>>::new();
    for reference in references {
        let Some(file_path) = module_output_paths.get(&reference.module_id) else {
            continue;
        };
        let Some(asset_output_path) = assets_by_logical_path.get(reference.logical_path.as_str())
        else {
            continue;
        };
        rewrites_by_file
            .entry(file_path.clone())
            .or_default()
            .insert(
                reference.logical_path.clone(),
                relative_asset_specifier(file_path.as_str(), asset_output_path),
            );
    }

    for file in &mut project.files {
        let Some(rewrites) = rewrites_by_file.get(file.path.as_str()) else {
            continue;
        };
        file.source =
            rewrite_string_literal_values(file.source.as_str(), file.path.as_str(), rewrites);
    }
}

fn canonicalize_emitted_source_locations(project: &mut EmittedProject) {
    for file in &mut project.files {
        file.source = rewrite_file_url_source_locations(file.source.as_str(), file.path.as_str());
    }
}

fn rewrite_file_url_source_locations(source: &str, path_hint: &str) -> String {
    let Ok(rewrites) = collect_file_url_source_location_rewrites(
        source,
        Some(Path::new(path_hint)),
        ParseGoal::TypeScript,
    ) else {
        return source.to_string();
    };
    let mut output = source.to_string();
    for rewrite in rewrites.iter().rev() {
        output.replace_range(
            rewrite.byte_start as usize..rewrite.byte_end as usize,
            "import.meta.url",
        );
    }
    output
}

fn rewrite_string_literal_values(
    source: &str,
    path_hint: &str,
    rewrites: &BTreeMap<String, String>,
) -> String {
    let Ok(literals) =
        collect_string_literals(source, Some(Path::new(path_hint)), ParseGoal::TypeScript)
    else {
        return source.to_string();
    };
    let mut output = source.to_string();
    for literal in literals.iter().rev() {
        let Some(replacement) = rewrites.get(literal.value.as_str()) else {
            continue;
        };
        output.replace_range(
            literal.byte_start as usize..literal.byte_end as usize,
            single_quoted_js_string(replacement).as_str(),
        );
    }
    output
}

fn single_quoted_js_string(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('\'');
    for ch in value.chars() {
        match ch {
            '\'' => output.push_str("\\'"),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            _ => output.push(ch),
        }
    }
    output.push('\'');
    output
}

fn module_output_paths(program: &EnrichedProgram) -> BTreeMap<ModuleId, String> {
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

fn relative_asset_specifier(from_file: &str, to_asset: &str) -> String {
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

fn audit_required_sources(program: &EnrichedProgram) -> AuditReport {
    let mut audit = AuditReport::default();
    for module in program.model().modules() {
        if module.kind == ModuleKind::Package {
            continue;
        }

        if has_module_source(program.model().input(), module.id) {
            continue;
        }

        let definitions = program.model().graph().definitions_for(module.id);
        if definitions.is_empty() {
            audit.push(
                AuditFinding::error(
                    FindingCode::MissingDefinition,
                    "module has no real source body to emit",
                )
                .with_module(module.id.0.to_string()),
            );
            continue;
        }

        for definition in definitions {
            audit.push(
                AuditFinding::error(
                    FindingCode::MissingDefinition,
                    "module has symbols but no real source body to emit",
                )
                .with_module(module.id.0.to_string())
                .with_binding(definition.as_str()),
            );
        }
    }
    audit
}

fn has_module_source(input: &InputBundle, module_id: ModuleId) -> bool {
    input.module_source_slice(module_id).is_some()
}

fn audit_emit_plan_synthesis(plan: &EmitPlan) -> AuditReport {
    let mut audit = AuditReport::default();
    for file in &plan.files {
        audit.extend(audit_file_synthesis(file));
    }
    audit
}

fn audit_file_synthesis(file: &PlannedFile) -> AuditReport {
    let mut audit = AuditReport::default();
    let declarations = planned_declarations(file);

    for binding in &file.bindings {
        if !binding.source_backed {
            audit.push(
                AuditFinding::error(
                    FindingCode::SyntheticReferenceWithoutDeclaration,
                    "planned binding has no recovered source declaration",
                )
                .with_module(file.path.clone())
                .with_binding(binding.emitted.as_str()),
            );
        }
    }

    for export in &file.exports {
        if export.source_backed {
            continue;
        }
        if !declarations.contains(&export.binding) {
            audit.push(
                AuditFinding::error(
                    FindingCode::SyntheticReferenceWithoutDeclaration,
                    "planned export references a binding without declaration or import",
                )
                .with_module(file.path.clone())
                .with_binding(export.binding.as_str()),
            );
        }
    }

    audit
}

fn planned_declarations(file: &PlannedFile) -> std::collections::BTreeSet<BindingName> {
    file.imports
        .iter()
        .filter(|import| !import.source_backed)
        .map(|import| import.namespace.clone())
        .chain(file.bindings.iter().map(|binding| binding.emitted.clone()))
        .collect()
}

fn audit_binding_shape_consistency(plan: &EmitPlan, project: &EmittedProject) -> AuditReport {
    let mut audit = AuditReport::default();
    for planned_file in &plan.files {
        let Some(emitted) = project
            .files
            .iter()
            .find(|file| file.path == planned_file.path)
        else {
            continue;
        };
        let classifications = classify_top_level_bindings(
            emitted.source.as_str(),
            Some(Path::new(emitted.path.as_str())),
            ParseGoal::TypeScript,
        );
        for binding in &planned_file.bindings {
            if !binding.source_backed || binding.shape != BindingShape::Callable {
                continue;
            }
            if classifications.get(binding.emitted.as_str())
                == Some(&DeclarationCallability::NotCallable)
            {
                audit.push(
                    AuditFinding::error(
                        FindingCode::CallableEmittedAsNonCallable,
                        "source-backed binding declared as a non-callable value is called like a function — likely a runtime error in the input",
                    )
                    .with_module(planned_file.path.clone())
                    .with_binding(binding.emitted.as_str()),
                );
            }
        }
    }
    audit
}

fn audit_emitted_project_parse(project: &EmittedProject) -> AuditReport {
    let mut audit = AuditReport::default();
    for file in &project.files {
        if let Err(error) = parse_source(
            file.source.as_str(),
            Some(Path::new(file.path.as_str())),
            ParseGoal::TypeScript,
        ) {
            audit.push(
                AuditFinding::error(
                    FindingCode::UnparseableOutput,
                    parse_error_message(&error, "output could not be parsed"),
                )
                .with_module(file.path.clone()),
            );
        }
    }
    audit
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineError {
    Plan(PlanError),
    Emit(EmitError),
}

impl fmt::Display for PipelineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plan(error) => write!(formatter, "{error}"),
            Self::Emit(error) => write!(formatter, "{error}"),
        }
    }
}

impl Error for PipelineError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Plan(source) => Some(source),
            Self::Emit(source) => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use reverts_analyze::enrich_program;
    use reverts_emitter::emit_project;
    use reverts_input::{
        AssetInput, AssetKind, InputBundle, InputRows, ModuleDependencyInput,
        ModuleDependencyTarget, ModuleInput, PackageAttributionInput, PackageSurfaceInput,
        ProjectInput, SourceFileInput, SourceSpan, SymbolInput,
    };
    use reverts_ir::{
        BindingName, BindingShape, BindingSourceKind, BindingUseKind, ModuleId, ModuleKind,
    };
    use reverts_js::{ParseGoal, parse_source};
    use reverts_model::ProgramModel;
    use reverts_observe::FindingCode;
    use reverts_planner::{EmitPlan, ImportExportPlanner, PlannedBinding, PlannedFile};

    use super::{
        OutputRun, audit_emit_plan_synthesis, collect_dynamic_asset_references,
        current_node_platform_dir, generate_project_from_input,
    };

    fn rows_with_application_module() -> InputRows {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "app", "src/index.ts"));
        rows
    }

    fn rows_with_application_source(source: &str) -> InputRows {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some(source.to_string()),
        ));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1));
        rows
    }

    #[test]
    fn complete_fixture_generates_parseable_project_with_package_match_and_semantic_names() {
        let mut rows = rows_with_application_source("export function activate() { return 42; }");
        rows.modules.push(ModuleInput::package(
            ModuleId(2),
            "lodash_map",
            "node_modules/lodash/map.js",
            "lodash",
            Some("4.17.21".to_string()),
        ));
        rows.symbols.push(SymbolInput::new(ModuleId(1), "activate"));
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(1),
            target: ModuleDependencyTarget::Package {
                specifier: "lodash/map".to_string(),
            },
        });
        rows.package_attributions.push(
            PackageAttributionInput::accepted_external(
                ModuleId(2),
                "lodash",
                "4.17.21",
                "lodash/map",
            )
            .with_subpath("map"),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        assert_eq!(run.project.files.len(), 1);
        assert_eq!(run.runtime_dependencies.len(), 1);
        assert_eq!(run.runtime_dependencies[0].package_name, "lodash");
        assert_eq!(run.runtime_dependencies[0].package_version, "4.17.21");
        let source = run.project.files[0].source.as_str();
        assert!(source.contains("import * as __pkg_lodash_map from 'lodash/map';"));
        assert!(source.contains("export function activate()"));
        assert!(!source.contains("undefined as any"));
    }

    #[test]
    fn package_surface_contributes_runtime_dependency_without_package_module() {
        let mut rows =
            rows_with_application_source("const undici = require('undici'); export { undici };");
        rows.package_surfaces
            .push(PackageSurfaceInput::accepted_external(
                "undici", "2.2.1", "undici",
            ));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        assert_eq!(run.runtime_dependencies.len(), 1);
        assert_eq!(run.runtime_dependencies[0].package_name, "undici");
        assert_eq!(run.runtime_dependencies[0].package_version, "2.2.1");
        assert!(run.project.files[0].source.contains("require('undici')"));
    }

    #[test]
    fn pipeline_carries_input_assets_to_output_run() {
        let mut rows = rows_with_application_source(
            "const rgPath = require('/$bunfs/root/vendor/rg'); export { rgPath };",
        );
        rows.assets.push(AssetInput::new(
            100,
            "/$bunfs/root/vendor/rg",
            "modules/1-app/vendor/rg",
            b"rg-binary".to_vec(),
            AssetKind::Executable,
            true,
        ));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        assert_eq!(run.assets.len(), 1);
        assert_eq!(run.assets[0].path, "modules/1-app/vendor/rg");
        assert_eq!(run.assets[0].bytes, b"rg-binary");
        assert!(run.assets[0].executable);
    }

    #[test]
    fn pipeline_rejects_asset_reference_missing_from_project_assets_without_fallback() {
        let rows = rows_with_application_source(
            "const native = require('/$bunfs/root/addon.node'); export { native };",
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should return audit");

        assert!(run.audit.has(FindingCode::MissingRequiredAsset));
        assert!(run.project.files.is_empty());
        assert!(run.assets.is_empty());
        assert!(run.audit.findings().iter().any(|finding| {
            finding.code == FindingCode::MissingRequiredAsset
                && finding.module.as_deref() == Some("1")
                && finding.binding.as_deref() == Some("/$bunfs/root/addon.node")
        }));
    }

    #[test]
    fn pipeline_emits_only_assets_referenced_by_source_literals() {
        let mut rows = rows_with_application_source(
            "const native = require('/$bunfs/root/addon.node'); export { native };",
        );
        rows.assets.push(AssetInput::new(
            100,
            "/$bunfs/root/addon.node",
            "modules/1-app/addon.node",
            b"native".to_vec(),
            AssetKind::NativeNode,
            false,
        ));
        rows.assets.push(AssetInput::new(
            101,
            "/$bunfs/root/unused.node",
            "modules/1-app/unused.node",
            b"unused".to_vec(),
            AssetKind::NativeNode,
            false,
        ));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        assert_eq!(run.assets.len(), 1);
        assert_eq!(run.assets[0].path, "modules/1-app/addon.node");
        assert_eq!(run.assets[0].bytes, b"native");
    }

    #[test]
    fn asset_audit_ignores_inert_string_literals_and_bare_commands() {
        let rows = rows_with_application_source(
            "const message = 'install bash.exe from C:\\\\Program Files\\\\Git\\\\bin\\\\bash.exe';\n\
             const cssSuffix = '.bundle.css';\n\
             const command = 'rg';\n\
             export { message, cssSuffix, command };",
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        assert!(run.assets.is_empty());
    }

    #[test]
    fn asset_audit_recovers_dynamic_ripgrep_vendor_binary_path() {
        let source = "\
            const root = path.resolve(base, 'vendor', 'ripgrep');\n\
            const command = path.resolve(root, `${process.arch}-${process.platform}`, 'rg');\n\
            export { command };";
        let references = collect_dynamic_asset_references(source, "fixture.ts");
        let Some(platform_dir) = current_node_platform_dir() else {
            return;
        };

        assert_eq!(
            references,
            vec![format!("vendor/ripgrep/{platform_dir}/rg")]
        );
    }

    #[test]
    fn asset_audit_recovers_hardcoded_ripgrep_vendor_binary_path_from_path_builders() {
        let source = "\
            const root = ODH.resolve(base, 'vendor', 'ripgrep');\n\
            const command = ODH.resolve(root, 'x64-linux', 'rg');\n\
            const inert = ['vendor', 'ripgrep', 'rg'];\n\
            export { command };";

        let references = collect_dynamic_asset_references(source, "fixture.ts");

        assert_eq!(references, vec!["vendor/ripgrep/x64-linux/rg".to_string()]);
    }

    #[test]
    fn asset_audit_keeps_ripgrep_binary_name_bound_to_its_platform_call() {
        let source = "\
            const root = path.resolve(base, 'vendor', 'ripgrep');\n\
            const command = process.platform === 'win32'\n\
                ? path.resolve(root, 'x64-win32', 'rg.exe')\n\
                : path.resolve(root, `${process.arch}-${process.platform}`, 'rg');\n\
            export { command };";
        let references = collect_dynamic_asset_references(source, "fixture.ts");
        let Some(platform_dir) = current_node_platform_dir() else {
            return;
        };

        assert_eq!(
            references,
            vec![
                format!("vendor/ripgrep/{platform_dir}/rg"),
                "vendor/ripgrep/x64-win32/rg.exe".to_string(),
            ]
        );
        assert!(
            !references
                .iter()
                .any(|reference| reference == "vendor/ripgrep/x64-win32/rg")
        );
    }

    #[test]
    fn pipeline_rewrites_matched_asset_literals_to_relative_emitted_paths() {
        let mut rows = rows_with_application_source(
            "const native = require('/$bunfs/root/addon.node'); export { native };",
        );
        rows.assets.push(AssetInput::new(
            100,
            "/$bunfs/root/addon.node",
            "src/addon.node",
            b"native".to_vec(),
            AssetKind::NativeNode,
            false,
        ));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        let source = run.project.files[0].source.as_str();
        assert!(source.contains("require('./addon.node')"));
        assert!(!source.contains("/$bunfs/root/addon.node"));
        assert_eq!(run.assets[0].path, "src/addon.node");
    }

    #[test]
    fn pipeline_recognizes_asset_reference_via_new_url_with_import_meta_url() {
        // The `new URL('X', import.meta.url)` idiom is the modern ESM way to
        // reference a sibling asset (wasm, native bindings). The detector
        // must accept the first-argument string as an asset specifier.
        let mut rows = rows_with_application_source(
            "const wasm = new URL('/$bunfs/root/parser.wasm', import.meta.url);\n\
             export { wasm };",
        );
        rows.assets.push(AssetInput::new(
            100,
            "/$bunfs/root/parser.wasm",
            "src/parser.wasm",
            b"\x00asm\x01\x00\x00\x00".to_vec(),
            AssetKind::Wasm,
            false,
        ));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings(),);
        assert_eq!(run.assets.len(), 1);
        assert_eq!(run.assets[0].path, "src/parser.wasm");
        let source = run.project.files[0].source.as_str();
        assert!(
            source.contains("new URL('./parser.wasm', import.meta.url)"),
            "new URL specifier must be rewritten to relative path; got:\n{source}",
        );
    }

    #[test]
    fn pipeline_recognizes_asset_reference_via_static_import_statement() {
        // ESM modules commonly reference native bindings via static imports
        // rather than CommonJS `require`. The asset detector must accept the
        // import specifier as an asset reference and rewrite it.
        let mut rows = rows_with_application_source(
            "import addon from '/$bunfs/root/addon.node';\nexport { addon };",
        );
        rows.assets.push(AssetInput::new(
            100,
            "/$bunfs/root/addon.node",
            "src/addon.node",
            b"native".to_vec(),
            AssetKind::NativeNode,
            false,
        ));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings(),);
        assert_eq!(run.assets.len(), 1);
        let source = run.project.files[0].source.as_str();
        assert!(
            source.contains("from './addon.node'"),
            "static import must be rewritten to relative path; got:\n{source}",
        );
        assert!(
            !source.contains("/$bunfs/root/addon.node"),
            "rewritten source must not retain original logical path; got:\n{source}",
        );
    }

    #[test]
    fn pipeline_emits_shared_asset_once_when_referenced_by_multiple_modules() {
        // Two application modules reference the same on-disk asset. The
        // pipeline must:
        //   - emit the asset bytes exactly once,
        //   - rewrite the literal in BOTH modules to a relative path.
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/loader-a.ts",
            Some("const a = require('/$bunfs/root/addon.node'); export { a };".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            2,
            "src/loader-b.ts",
            Some("const b = require('/$bunfs/root/addon.node'); export { b };".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "loader_a", "src/loader-a.ts")
                .with_source_file(1),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "loader_b", "src/loader-b.ts")
                .with_source_file(2),
        );
        rows.assets.push(AssetInput::new(
            100,
            "/$bunfs/root/addon.node",
            "src/addon.node",
            b"native".to_vec(),
            AssetKind::NativeNode,
            false,
        ));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        assert_eq!(
            run.assets.len(),
            1,
            "shared asset must not be emitted multiple times: {:?}",
            run.assets,
        );
        assert_eq!(run.assets[0].path, "src/addon.node");
        assert_eq!(run.assets[0].bytes, b"native");

        assert_eq!(run.project.files.len(), 2);
        for file in &run.project.files {
            assert!(
                file.source.contains("require('./addon.node')"),
                "module {} must rewrite to relative path; got:\n{}",
                file.path,
                file.source,
            );
            assert!(
                !file.source.contains("/$bunfs/root/addon.node"),
                "module {} must not retain the original logical path; got:\n{}",
                file.path,
                file.source,
            );
        }
    }

    #[test]
    fn pipeline_canonicalizes_build_file_url_source_locations_for_asset_paths() {
        let mut rows = rows_with_application_source(
            "\
            const POL = { fileURLToPath(value) { return value; } };\n\
            const ODH = { join(...parts) { return parts.join('/'); }, resolve(...parts) { return parts.join('/'); } };\n\
            const yL9 = POL.fileURLToPath('file:///home/runner/work/claude-cli-internal/claude-cli-internal/src/utils/ripgrep.ts');\n\
            const hL9 = ODH.join(yL9, '../');\n\
            const root = ODH.resolve(hL9, 'vendor', 'ripgrep');\n\
            const command = ODH.resolve(root, 'x64-linux', 'rg');\n\
            export { command };",
        );
        rows.assets.push(AssetInput::new(
            100,
            "vendor/ripgrep/x64-linux/rg",
            "src/vendor/ripgrep/x64-linux/rg",
            b"rg".to_vec(),
            AssetKind::Executable,
            true,
        ));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        assert_eq!(run.assets.len(), 1);
        assert_eq!(run.assets[0].path, "src/vendor/ripgrep/x64-linux/rg");
        let source = run.project.files[0].source.as_str();
        assert!(source.contains("POL.fileURLToPath(import.meta.url)"));
        assert!(!source.contains("/home/runner/work/claude-cli-internal"));
    }

    #[test]
    fn source_backed_package_import_is_not_emitted_twice() {
        let mut rows = rows_with_application_source(
            "import { map } from 'lodash/map'; export const answer = map;",
        );
        rows.modules.push(ModuleInput::package(
            ModuleId(2),
            "lodash_map",
            "node_modules/lodash/map.js",
            "lodash",
            Some("4.17.21".to_string()),
        ));
        rows.package_attributions.push(
            PackageAttributionInput::accepted_external(
                ModuleId(2),
                "lodash",
                "4.17.21",
                "lodash/map",
            )
            .with_subpath("map"),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        let source = run.project.files[0].source.as_str();
        assert!(source.contains("import { map } from 'lodash/map';"));
        assert!(!source.contains("__pkg_lodash_map"));
        assert_eq!(source.matches("from 'lodash/map'").count(), 1);
    }

    #[test]
    fn commonjs_require_package_import_uses_surface_attribution_without_synthetic_import() {
        let mut rows = rows_with_application_source(
            "const add = require('pkg/add'); export const total = add(1, 2);",
        );
        rows.modules.push(ModuleInput::package(
            ModuleId(2),
            "pkg_add",
            "node_modules/pkg/add.js",
            "pkg",
            Some("1.2.3".to_string()),
        ));
        rows.package_attributions.push(
            PackageAttributionInput::accepted_external(ModuleId(2), "pkg", "1.2.3", "pkg/add")
                .with_subpath("add"),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        let source = run.project.files[0].source.as_str();
        assert!(source.contains("require('pkg/add')"));
        assert!(!source.contains("__pkg_pkg_add"));
    }

    #[test]
    fn source_backed_default_export_does_not_require_synthetic_default_binding() {
        let rows = rows_with_application_source("export default () => 42;");
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        assert_eq!(run.project.files.len(), 1);
        assert!(
            run.project.files[0]
                .source
                .contains("export default () => 42")
        );
    }

    #[test]
    fn paper_aligned_pipeline_wires_graph_shape_package_plan_emit_and_parse() {
        let mut rows = rows_with_application_source(
            r#"
                import { factory } from 'pkg/factory';
                const bag = { value: 1 };
                function local() {
                    return factory(bag.value);
                }
                local();
                export { local };
            "#,
        );
        rows.modules.push(ModuleInput::package(
            ModuleId(2),
            "pkg_factory",
            "node_modules/pkg/factory.js",
            "pkg",
            Some("1.2.3".to_string()),
        ));
        rows.package_attributions.push(
            PackageAttributionInput::accepted_external(ModuleId(2), "pkg", "1.2.3", "pkg/factory")
                .with_subpath("factory"),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let model = ProgramModel::from_input(input);
        let graph = model.graph();
        assert!(graph.ast_errors().is_empty());
        assert!(
            graph
                .import_export()
                .package_imports_for(ModuleId(1))
                .contains(&"pkg/factory")
        );
        assert!(
            graph
                .import_export()
                .exports_for(ModuleId(1))
                .iter()
                .any(|binding| binding.as_str() == "local")
        );
        assert!(graph.def_use().unresolved_reads().is_empty());
        assert!(graph.def_use().unresolved_writes().is_empty());
        let dependence_edges = graph.def_use().data_dependence_edges();
        assert!(dependence_edges.iter().any(|edge| {
            edge.binding.as_str() == "factory"
                && edge.source == BindingSourceKind::Import
                && edge.target == BindingUseKind::Read
        }));
        assert!(dependence_edges.iter().any(|edge| {
            edge.binding.as_str() == "bag"
                && edge.source == BindingSourceKind::Definition
                && edge.target == BindingUseKind::Read
        }));

        let enrichment = enrich_program(model);
        assert!(enrichment.audit.is_clean());
        let program = enrichment.program;
        assert_eq!(
            program.binding_shape(ModuleId(1), "factory"),
            BindingShape::Callable
        );
        assert_eq!(
            program.binding_shape(ModuleId(1), "local"),
            BindingShape::Callable
        );
        assert_eq!(
            program.binding_shape(ModuleId(1), "bag"),
            BindingShape::NamespaceObject
        );
        assert_eq!(program.package_imports().len(), 1);
        assert!(program.package_imports()[0].source_backed);
        assert_eq!(
            program.package_imports()[0].resolution.specifier(),
            Some("pkg/factory")
        );

        let plan = ImportExportPlanner
            .plan_enriched_program(&program)
            .expect("paper-aligned fixture should plan");
        let file = &plan.files[0];
        assert!(
            file.imports.iter().any(|import| import.source_backed
                && import.resolution.specifier() == Some("pkg/factory"))
        );
        assert!(
            file.bindings
                .iter()
                .any(|binding| binding.original.as_str() == "factory" && binding.source_backed)
        );
        assert!(
            file.exports
                .iter()
                .any(|export| export.binding.as_str() == "local" && export.source_backed)
        );

        let project = emit_project(&plan).expect("paper-aligned fixture should emit");
        let source = project.files[0].source.as_str();
        assert!(source.contains("import { factory } from 'pkg/factory';"));
        assert!(source.contains("function local()"));
        assert!(source.contains("export { local };"));
        assert!(!source.contains("__pkg_pkg_factory"));
        parse_source(
            source,
            Some(Path::new(project.files[0].path.as_str())),
            ParseGoal::TypeScript,
        )
        .expect("emitted fixture should parse");
    }

    #[test]
    fn pipeline_wires_arbitrary_bundle_prelude_runtime_helpers_end_to_end() {
        let prelude = concat!(
            "var $wrap7 = (factory, cache) => () => ",
            "(cache || factory((cache = { exports: {} }).exports, cache), cache.exports);\n",
            "var _lazy9 = (init, cache) => () => (init && (cache = init(init = 0)), cache);\n",
        );
        let body = concat!(
            "var entry = $wrap7((exports, module) => { module.exports = 1; });\n",
            "var init = _lazy9(() => { entry(); });\n",
            "init();\n",
            "export { entry };\n",
        );
        let source = format!("{prelude}{body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        assert_eq!(run.project.files.len(), 1);
        let entry = run
            .project
            .files
            .iter()
            .find(|file| file.path == "modules/entry.ts")
            .expect("entry file should be emitted");
        assert!(!entry.source.contains("$wrap7"));
        assert!(!entry.source.contains("_lazy9"));
        assert!(entry.source.contains("__reverts_cached_module"));
        assert!(entry.source.contains("__reverts_initialized"));
    }

    #[test]
    fn source_backed_reexports_stay_on_package_surface_without_synthetic_namespace() {
        let mut rows = rows_with_application_source(
            "export { map as lodashMap } from 'lodash/map';\nexport * as fp from 'lodash/fp';",
        );
        rows.modules.push(ModuleInput::package(
            ModuleId(2),
            "lodash_map",
            "node_modules/lodash/map.js",
            "lodash",
            Some("4.17.21".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(3),
            "lodash_fp",
            "node_modules/lodash/fp.js",
            "lodash",
            Some("4.17.21".to_string()),
        ));
        rows.package_attributions.push(
            PackageAttributionInput::accepted_external(
                ModuleId(2),
                "lodash",
                "4.17.21",
                "lodash/map",
            )
            .with_subpath("map"),
        );
        rows.package_attributions.push(
            PackageAttributionInput::accepted_external(
                ModuleId(3),
                "lodash",
                "4.17.21",
                "lodash/fp",
            )
            .with_subpath("fp"),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        let source = run.project.files[0].source.as_str();
        assert!(source.contains("export { map as lodashMap } from 'lodash/map';"));
        assert!(source.contains("export * as fp from 'lodash/fp';"));
        assert!(!source.contains("__pkg_lodash"));
        assert_eq!(source.matches("from 'lodash/").count(), 2);
    }

    #[test]
    fn unknown_package_surface_reports_unresolvable_import_without_external_tools() {
        let mut rows = rows_with_application_module();
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(1),
            target: ModuleDependencyTarget::Package {
                specifier: "lodash/map".to_string(),
            },
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.has(FindingCode::UnresolvableBareImport));
        assert!(run.project.files.is_empty());
    }

    #[test]
    fn missing_symbol_source_is_reported_without_emitting_generated_implementation() {
        let mut rows = rows_with_application_module();
        rows.symbols.push(SymbolInput::new(ModuleId(1), "activate"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.has(FindingCode::MissingDefinition));
        assert!(run.project.files.is_empty());
    }

    #[test]
    fn module_without_source_is_reported_without_emitting_empty_file() {
        let rows = rows_with_application_module();
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.findings().iter().any(|finding| {
            finding.code == FindingCode::MissingDefinition
                && finding.module.as_deref() == Some("1")
                && finding.binding.is_none()
        }));
        assert!(run.project.files.is_empty());
    }

    #[test]
    fn duplicate_input_symbols_emit_single_missing_source_finding() {
        let mut rows = rows_with_application_module();
        rows.symbols.push(SymbolInput::new(ModuleId(1), "activate"));
        rows.symbols.push(SymbolInput::new(ModuleId(1), "activate"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert_eq!(
            run.audit
                .findings()
                .iter()
                .filter(|finding| finding.code == FindingCode::MissingDefinition
                    && finding.binding.as_deref() == Some("activate"))
                .count(),
            1
        );
        assert!(run.project.files.is_empty());
    }

    #[test]
    fn shared_bundle_source_is_not_duplicated_as_module_implementation() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some("function bundle() {}".to_string()),
        ));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "m1", "modules/m1.ts").with_source_file(1));
        rows.modules.push(ModuleInput {
            id: ModuleId(2),
            kind: ModuleKind::Application,
            original_name: "m2".to_string(),
            semantic_path: "modules/m2.ts".to_string(),
            source_file_id: Some(1),
            source_span: None,
            package_name: None,
            package_version: None,
        });
        rows.symbols.push(SymbolInput::new(ModuleId(1), "one"));
        rows.symbols.push(SymbolInput::new(ModuleId(2), "two"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.has(FindingCode::MissingDefinition));
        assert!(run.project.files.is_empty());
    }

    #[test]
    fn bundle_source_spans_emit_real_module_slices() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some("export const one = 1;\nexport const two = 2;".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "one", "modules/one.ts")
                .with_source_file(1)
                .with_source_span(reverts_input::SourceSpan::new(0, 21)),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "two", "modules/two.ts")
                .with_source_file(1)
                .with_source_span(reverts_input::SourceSpan::new(22, 43)),
        );
        rows.symbols.push(SymbolInput::new(ModuleId(1), "one"));
        rows.symbols.push(SymbolInput::new(ModuleId(2), "two"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        assert_eq!(run.project.files.len(), 2);
        assert!(run.project.files[0].source.contains("export const one = 1"));
        assert!(run.project.files[1].source.contains("export const two = 2"));
    }

    #[test]
    fn source_module_symbol_hint_not_recovered_from_ast_is_not_synthesized() {
        let mut rows = rows_with_application_source("export const real = 1;");
        rows.symbols.push(SymbolInput::new(ModuleId(1), "missing"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        assert_eq!(run.project.files.len(), 1);
        assert!(!run.project.files[0].source.contains("missing"));
    }

    #[test]
    fn unresolved_ast_read_is_rejected_by_first_audit_gate() {
        let rows = rows_with_application_source("missing();");
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should return audit");

        assert!(run.audit.has(FindingCode::MissingDefinition));
        assert!(run.project.files.is_empty());
    }

    #[test]
    fn synthesis_audit_accepts_declared_export_and_rejects_missing_export_binding() {
        let mut file = PlannedFile::new("src/index.ts");
        file.add_binding(PlannedBinding::new(
            BindingName::new("real"),
            BindingName::new("real"),
            BindingShape::Unknown,
            true,
        ));
        file.add_export(BindingName::new("real"));
        file.add_export(BindingName::new("missing"));
        let mut plan = EmitPlan::default();
        plan.push_file(file);

        let audit = audit_emit_plan_synthesis(&plan);

        assert_eq!(audit.findings().len(), 1);
        assert_eq!(
            audit.findings()[0].code,
            FindingCode::SyntheticReferenceWithoutDeclaration
        );
        assert_eq!(audit.findings()[0].binding.as_deref(), Some("missing"));
    }

    #[test]
    fn callable_shape_emitted_as_value_declaration_is_reported_by_pipeline() {
        // The bundle defines `runner` as a plain value but also calls it. The shape
        // solver upgrades `runner` to Callable; the audit must notice that the
        // emitted source declares `runner` as a non-callable variable and report.
        let source = "const runner = 42;\nrunner();\n";
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/runner.ts",
            Some(source.to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "runner", "src/runner.ts").with_source_file(1),
        );
        rows.symbols.push(SymbolInput::new(ModuleId(1), "runner"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        let finding = run
            .audit
            .findings()
            .iter()
            .find(|finding| finding.code == FindingCode::CallableEmittedAsNonCallable)
            .expect("expected CallableEmittedAsNonCallable finding");
        assert!(
            finding.message.contains("source-backed"),
            "finding must blame the source, not the emitter; got: {:?}",
            finding.message,
        );
        assert!(
            finding.message.contains("called like a function"),
            "finding must describe the runtime-error symptom; got: {:?}",
            finding.message,
        );
    }

    #[test]
    fn enum_iife_source_solves_to_enum_object_shape_and_preserves_body_in_output() {
        let enum_source = "var Color;\n\
            (function (Color) {\n\
                Color[Color[\"Red\"] = 0] = \"Red\";\n\
                Color[Color[\"Green\"] = 1] = \"Green\";\n\
            })(Color || (Color = {}));\n";
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/colors.ts",
            Some(enum_source.to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "colors", "src/colors.ts").with_source_file(1),
        );
        rows.symbols.push(SymbolInput::new(ModuleId(1), "Color"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let model = ProgramModel::from_input(input.clone());
        let enrichment = enrich_program(model);
        assert_eq!(
            enrichment.program.binding_shape(ModuleId(1), "Color"),
            BindingShape::EnumObject,
            "enum-IIFE source must solve to EnumObject shape",
        );

        let run = generate_project_from_input(input).expect("fixture should emit");
        assert!(
            run.audit.is_clean(),
            "expected clean audit, got: {:?}",
            run.audit.findings(),
        );
        assert_eq!(run.project.files.len(), 1);
        let emitted = run.project.files[0].source.as_str();
        assert!(
            emitted.contains("Color[Color['Red'] = 0]"),
            "emitted source must preserve enum reverse-mapping body, got:\n{emitted}",
        );
        assert!(
            emitted.contains("Color[Color['Green'] = 1]"),
            "emitted source must preserve all enum members, got:\n{emitted}",
        );
    }

    #[test]
    fn webpack_bundle_emits_compiler_specific_recovery_banner() {
        // The fixture has the canonical __webpack_require__ identifier at module
        // scope, so compiler detection classifies the module as Webpack and the
        // pipeline must surface that decision via a recovery banner.
        let source = "var __webpack_require__ = function (id) { return id; };\n\
                      var entry = __webpack_require__(1);\n";
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/runtime.ts",
            Some(source.to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "runtime", "src/runtime.ts").with_source_file(1),
        );
        rows.symbols
            .push(SymbolInput::new(ModuleId(1), "__webpack_require__"));
        rows.symbols.push(SymbolInput::new(ModuleId(1), "entry"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");
        assert!(
            run.audit.is_clean(),
            "expected clean audit, got: {:?}",
            run.audit.findings(),
        );
        let emitted = run.project.files[0].source.as_str();
        assert!(
            emitted.contains("// reverts-recovery: webpack"),
            "emitted source must carry a webpack recovery banner, got:\n{emitted}",
        );
    }

    #[test]
    fn webpack_runtime_identifier_inside_function_still_classifies_as_webpack() {
        // Real webpack bundles wrap their runtime in an IIFE; identifiers like
        // __webpack_require__ live inside function bodies and are filtered out
        // of AST facts by the module-scope rule. The compiler detector must
        // fall back to raw-source scanning so these bundles still get classified.
        let source = "function activate() {\n  __webpack_require__(1);\n  return 42;\n}\n";
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/runtime.ts",
            Some(source.to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "runtime", "src/runtime.ts").with_source_file(1),
        );
        rows.symbols.push(SymbolInput::new(ModuleId(1), "activate"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");
        assert!(
            run.audit.is_clean(),
            "expected clean audit, got: {:?}",
            run.audit.findings(),
        );
        let emitted = run.project.files[0].source.as_str();
        assert!(
            emitted.contains("// reverts-recovery: webpack"),
            "in-function webpack runtime identifier must still trigger webpack banner, got:\n{emitted}",
        );
    }

    #[test]
    fn unknown_compiler_does_not_emit_recovery_banner() {
        // Plain TypeScript source with no bundler signals must NOT carry a
        // recovery banner; the banner is reserved for non-Unknown compilers.
        let source = "export function add(a: number, b: number) {\n  return a + b;\n}\n";
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/add.ts",
            Some(source.to_string()),
        ));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "add", "src/add.ts").with_source_file(1));
        rows.symbols.push(SymbolInput::new(ModuleId(1), "add"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");
        let emitted = run.project.files[0].source.as_str();
        assert!(
            !emitted.contains("reverts-recovery"),
            "unknown-compiler module must not include a recovery banner, got:\n{emitted}",
        );
    }

    fn run_with_source(source: &str, symbols: &[&str]) -> OutputRun {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/bundle.ts",
            Some(source.to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "bundle", "src/bundle.ts").with_source_file(1),
        );
        for symbol in symbols {
            rows.symbols.push(SymbolInput::new(ModuleId(1), *symbol));
        }
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        generate_project_from_input(input).expect("fixture should emit")
    }

    #[test]
    fn esbuild_runtime_identifier_in_function_emits_esbuild_banner() {
        let source = "function setup() {\n  __toCommonJS({});\n}\n";
        let run = run_with_source(source, &["setup"]);
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            emitted.contains("// reverts-recovery: esbuild"),
            "esbuild fixture must carry esbuild banner, got:\n{emitted}",
        );
    }

    #[test]
    fn rollup_object_freeze_pattern_emits_rollup_banner() {
        let source = "var frozen = Object.freeze({ answer: 42 });\n";
        let run = run_with_source(source, &["frozen"]);
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            emitted.contains("// reverts-recovery: rollup"),
            "rollup fixture must carry rollup banner, got:\n{emitted}",
        );
    }

    #[test]
    fn babel_interop_helper_in_function_emits_babel_banner() {
        let source = "function load(mod) {\n  return _interopRequireDefault(mod);\n}\n";
        let run = run_with_source(source, &["load"]);
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            emitted.contains("// reverts-recovery: babel"),
            "babel fixture must carry babel banner, got:\n{emitted}",
        );
    }

    #[test]
    fn babel_es_module_marker_is_stripped_from_emitted_output() {
        // The `Object.defineProperty(exports, "__esModule", { value: true })`
        // statement is a no-op in ESM output. When the module is classified
        // as Babel, the lowering pipeline must strip it from the emit.
        let source = "Object.defineProperty(exports, \"__esModule\", { value: true });\n\
                      function load(mod) {\n  return _interopRequireDefault(mod);\n}\n";
        let run = run_with_source(source, &["load"]);
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            emitted.contains("// reverts-recovery: babel"),
            "babel fixture must keep its banner, got:\n{emitted}",
        );
        assert!(
            !emitted.contains("__esModule"),
            "babel lowering must strip the __esModule marker, got:\n{emitted}",
        );
        assert!(
            emitted.contains("function load"),
            "babel lowering must keep unrelated declarations, got:\n{emitted}",
        );
    }

    #[test]
    fn babel_interop_require_default_call_is_rewritten_to_default_wrapped_require() {
        // Babel's classic CJS interop pattern wraps a require with the
        // `_interopRequireDefault` helper:
        //     var _foo = _interopRequireDefault(require("./foo"));
        // After lowering the helper call is dropped and the original require
        // is wrapped in a plain `{ default: ... }` literal — that preserves
        // every existing `_foo.default` access without keeping the helper
        // around.
        let source = "var _foo = _interopRequireDefault(require('./foo'));\n\
                      function _interopRequireDefault(e) { return e && e.__esModule ? e : { default: e }; }\n\
                      var entry = _foo.default;\n";
        let run = run_with_source(source, &["_foo", "_interopRequireDefault", "entry"]);
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            emitted.contains("// reverts-recovery: babel"),
            "babel fixture must keep its banner; got:\n{emitted}",
        );
        assert!(
            emitted.contains("var _foo = { default: require('./foo') }"),
            "babel lowering must rewrite the helper call into a literal default wrapper; got:\n{emitted}",
        );
        assert!(
            !emitted.contains("_interopRequireDefault(require("),
            "babel lowering must drop the helper *call* (its definition may remain unused); got:\n{emitted}",
        );
        assert!(
            emitted.contains("var entry = _foo.default"),
            "babel lowering must preserve subsequent .default accesses; got:\n{emitted}",
        );
        assert!(
            !emitted.contains("function _interopRequireDefault"),
            "babel lowering must strip the helper definition once all calls are rewritten; got:\n{emitted}",
        );
    }

    #[test]
    fn babel_interop_require_default_helper_is_kept_when_a_call_remains_unrewritten() {
        // The helper definition may only be stripped when nothing in the
        // emitted output still references it. A non-rewritable use (e.g.
        // a top-level call without the var-init pattern) must keep the
        // helper alive so the runtime stays valid.
        let source = "var _foo = _interopRequireDefault(require('./foo'));\n\
                      function _interopRequireDefault(e) { return e && e.__esModule ? e : { default: e }; }\n\
                      var _bar = _interopRequireDefault({});\n\
                      var entry = _foo.default;\n\
                      var bar = _bar.default;\n";
        let run = run_with_source(
            source,
            &["_foo", "_interopRequireDefault", "_bar", "entry", "bar"],
        );
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            emitted.contains("function _interopRequireDefault"),
            "helper definition must remain because at least one call site survives; got:\n{emitted}",
        );
        assert!(
            emitted.contains("_interopRequireDefault({})"),
            "untouched call must still be present; got:\n{emitted}",
        );
    }

    #[test]
    fn terser_minified_source_emits_terser_banner() {
        // Long single-line source with low whitespace ratio triggers looks_minified
        // without matching any specific compiler runtime identifier.
        let source = "var a=function(b){return b?b.c?b.c.d:b.c:b};var c={};for(var d=0;d<200;d++)c[d]=a({c:{d:d}});var e=function(f){return f&&f.g?f.g.h:0};var x=c[0];var y=e(x);\n";
        let run = run_with_source(source, &["a", "c", "d", "e", "x", "y"]);
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            emitted.contains("// reverts-recovery: terser"),
            "terser fixture must carry terser banner, got:\n{emitted}",
        );
    }
}
