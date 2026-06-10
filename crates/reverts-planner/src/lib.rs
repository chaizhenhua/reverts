use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::path::Path;

use reverts_graph::{AstFactKind, RevertsGraph};
use reverts_ir::{BindingName, BindingShape, ModuleId, ModuleKind};
use reverts_js::{JsError, ParseGoal, format_source_pretty};
use reverts_model::{CompilerKind, EnrichedProgram, ModuleCompilerProfile};
use reverts_package::PackageResolution;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EmitPlan {
    pub files: Vec<PlannedFile>,
}

impl EmitPlan {
    pub fn push_file(&mut self, file: PlannedFile) {
        self.files.push(file);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    pub path: String,
    pub imports: Vec<PlannedImport>,
    pub bindings: Vec<PlannedBinding>,
    pub exports: Vec<PlannedExport>,
    pub body: Vec<String>,
    pub source_strategy: SourceCompilerStrategy,
}

impl PlannedFile {
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            imports: Vec::new(),
            bindings: Vec::new(),
            exports: Vec::new(),
            body: Vec::new(),
            source_strategy: SourceCompilerStrategy::DirectSource,
        }
    }

    pub fn add_import(&mut self, import: PlannedImport) {
        self.imports.push(import);
    }

    pub fn add_binding(&mut self, binding: PlannedBinding) {
        self.bindings.push(binding);
    }

    pub fn add_export(&mut self, binding: BindingName) {
        self.add_export_with_source_backed(binding, false);
    }

    pub fn add_export_with_source_backed(&mut self, binding: BindingName, source_backed: bool) {
        self.exports.push(PlannedExport {
            binding,
            source_backed,
        });
    }

    pub fn push_source(&mut self, source: impl Into<String>) {
        self.body.push(source.into());
    }

    pub fn set_source_strategy(&mut self, source_strategy: SourceCompilerStrategy) {
        self.source_strategy = source_strategy;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedImport {
    pub namespace: BindingName,
    pub resolution: PackageResolution,
    pub source_backed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedBinding {
    pub original: BindingName,
    pub emitted: BindingName,
    pub shape: BindingShape,
    pub source_backed: bool,
}

impl PlannedBinding {
    #[must_use]
    pub fn new(
        original: BindingName,
        emitted: BindingName,
        shape: BindingShape,
        source_backed: bool,
    ) -> Self {
        Self {
            original,
            emitted,
            shape,
            source_backed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedExport {
    pub binding: BindingName,
    pub source_backed: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum SourceCompilerStrategy {
    #[default]
    DirectSource,
    WebpackRuntime,
    EsbuildHelpers,
    RollupFacade,
    BabelTranspiled,
    TerserMinified,
}

impl SourceCompilerStrategy {
    #[must_use]
    pub fn from_profile(profile: &ModuleCompilerProfile) -> Self {
        match profile.compiler {
            CompilerKind::Unknown => Self::DirectSource,
            CompilerKind::Webpack => Self::WebpackRuntime,
            CompilerKind::Esbuild => Self::EsbuildHelpers,
            CompilerKind::Rollup => Self::RollupFacade,
            CompilerKind::Babel => Self::BabelTranspiled,
            CompilerKind::Terser => Self::TerserMinified,
        }
    }

    #[must_use]
    pub const fn parse_goal(self) -> ParseGoal {
        match self {
            Self::DirectSource => ParseGoal::TypeScript,
            Self::WebpackRuntime
            | Self::EsbuildHelpers
            | Self::RollupFacade
            | Self::BabelTranspiled
            | Self::TerserMinified => ParseGoal::JavaScript,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ImportExportPlanner;

impl ImportExportPlanner {
    pub fn plan_enriched_program(self, program: &EnrichedProgram) -> Result<EmitPlan, PlanError> {
        let mut plan = EmitPlan::default();

        for module in program.model().modules() {
            if module.kind == ModuleKind::Package {
                continue;
            }

            let path = program
                .semantic_names()
                .module_path(module.id)
                .unwrap_or(module.semantic_path.as_str());
            let mut file = PlannedFile::new(path);
            let compiler_profile = program.compiler_profile().module(module.id);
            let source_strategy = SourceCompilerStrategy::from_profile(&compiler_profile);
            file.set_source_strategy(source_strategy);

            for decision in program.package_imports_for(module.id) {
                file.add_import(PlannedImport {
                    namespace: decision.namespace_binding.clone(),
                    resolution: decision.resolution.clone(),
                    source_backed: decision.source_backed,
                });
            }

            let source_definitions = ast_definitions_for(program.model().graph(), module.id);
            let source_imports = ast_imports_for(program.model().graph(), module.id);
            let mut planned_bindings = BTreeSet::<BindingName>::new();
            for original in program.model().graph().definitions_for(module.id) {
                let emitted = program
                    .semantic_names()
                    .binding_name(module.id, original.as_str())
                    .cloned()
                    .unwrap_or_else(|| original.clone());
                let shape = program.binding_shape(module.id, original.as_str());
                let source_backed = source_definitions.contains(&original);
                planned_bindings.insert(original.clone());
                file.add_binding(PlannedBinding::new(original, emitted, shape, source_backed));
            }

            for original in source_imports {
                if planned_bindings.contains(&original) {
                    continue;
                }
                file.add_binding(PlannedBinding::new(
                    original.clone(),
                    original,
                    BindingShape::Unknown,
                    true,
                ));
            }

            if let Some(source) = program.model().input().module_source_slice(module.id) {
                let normalized = normalize_source_for_emit(
                    source.module_id,
                    source.source_file_path,
                    source.source,
                    source_strategy,
                )?;
                file.push_source(normalized);
            }

            for export in program
                .model()
                .graph()
                .import_export()
                .exports_for(module.id)
            {
                let emitted = program
                    .semantic_names()
                    .binding_name(module.id, export.as_str())
                    .cloned()
                    .unwrap_or(export);
                file.add_export_with_source_backed(emitted, true);
            }

            plan.push_file(file);
        }

        Ok(plan)
    }

    #[must_use]
    pub fn plan_module_file(
        self,
        graph: &RevertsGraph,
        module_id: ModuleId,
        path: impl Into<String>,
    ) -> PlannedFile {
        let mut file = PlannedFile::new(path);
        for binding in graph.definitions_for(module_id) {
            file.add_export(binding);
        }
        file
    }
}

fn ast_definitions_for(graph: &RevertsGraph, module_id: ModuleId) -> BTreeSet<BindingName> {
    graph
        .ast_facts()
        .iter()
        .filter(|fact| fact.module_id == module_id && fact.kind == AstFactKind::Definition)
        .filter_map(|fact| fact.binding.clone())
        .collect()
}

fn ast_imports_for(graph: &RevertsGraph, module_id: ModuleId) -> BTreeSet<BindingName> {
    graph
        .ast_facts()
        .iter()
        .filter(|fact| fact.module_id == module_id && fact.kind == AstFactKind::Import)
        .filter_map(|fact| fact.binding.clone())
        .collect()
}

fn normalize_source_for_emit(
    module_id: ModuleId,
    path: &str,
    source: &str,
    source_strategy: SourceCompilerStrategy,
) -> Result<String, PlanError> {
    format_source_pretty(
        source,
        source_path_hint(path, source_strategy),
        source_strategy.parse_goal(),
    )
    .map_err(|error| PlanError::UnparseableSource {
        module_id,
        path: path.to_string(),
        message: parse_error_message(&error),
    })
}

fn source_path_hint(path: &str, source_strategy: SourceCompilerStrategy) -> Option<&Path> {
    match source_strategy {
        SourceCompilerStrategy::DirectSource => Some(Path::new(path)),
        SourceCompilerStrategy::WebpackRuntime
        | SourceCompilerStrategy::EsbuildHelpers
        | SourceCompilerStrategy::RollupFacade
        | SourceCompilerStrategy::BabelTranspiled
        | SourceCompilerStrategy::TerserMinified => None,
    }
}

fn parse_error_message(error: &JsError) -> String {
    match error {
        JsError::ParseFailed(errors) => errors.first().map_or_else(
            || "source could not be parsed".to_string(),
            |error| {
                let diagnostic = error
                    .diagnostics
                    .first()
                    .map_or("no diagnostic", String::as_str);
                format!(
                    "source could not be parsed as {}: {diagnostic}",
                    error.source_type
                )
            },
        ),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    UnparseableSource {
        module_id: ModuleId,
        path: String,
        message: String,
    },
}

impl fmt::Display for PlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnparseableSource {
                module_id,
                path,
                message,
            } => write!(
                formatter,
                "module {} source {} failed normalization: {message}",
                module_id.0, path
            ),
        }
    }
}

impl Error for PlanError {}

#[cfg(test)]
mod tests {
    use reverts_input::{
        InputBundle, InputRows, ModuleInput, ProjectInput, SourceFileInput, SymbolInput,
    };
    use reverts_ir::{BindingShape, BindingShapeSolution, ModuleId};
    use reverts_model::{
        CompilerEvidence, CompilerKind, CompilerProfile, ModuleCompilerProfile, ProgramModel,
    };

    use super::{ImportExportPlanner, SourceCompilerStrategy};

    #[test]
    fn enriched_program_plans_real_source_without_synthetic_declarations() {
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("export const answer = 42;".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");

        assert_eq!(
            plan.files[0].body[0].trim_end(),
            "export const answer = 42;"
        );
    }

    #[test]
    fn enriched_program_normalizes_source_before_emit_plan() {
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("export function add(a,b){return a+b}".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");

        assert!(plan.files[0].body[0].contains("export function add(a, b)"));
        assert!(plan.files[0].body[0].contains("return a + b;"));
    }

    #[test]
    fn enriched_program_plans_real_source_slice_from_bundle_span() {
        let planner = ImportExportPlanner;
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
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");

        assert_eq!(plan.files[0].body[0].trim_end(), "export const one = 1;");
        assert_eq!(plan.files[1].body[0].trim_end(), "export const two = 2;");
    }

    #[test]
    fn compiler_profile_selects_webpack_source_strategy() {
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some("__webpack_require__(1);".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let mut compiler_profile = CompilerProfile::default();
        compiler_profile.insert_module(
            ModuleId(1),
            ModuleCompilerProfile::new(
                CompilerKind::Webpack,
                false,
                vec![CompilerEvidence::Identifier(
                    "__webpack_require__".to_string(),
                )],
            ),
        );
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        )
        .with_compiler_profile(compiler_profile);

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");

        assert_eq!(
            plan.files[0].source_strategy,
            SourceCompilerStrategy::WebpackRuntime
        );
        assert_eq!(plan.files[0].body[0].trim_end(), "__webpack_require__(1);");
    }

    #[test]
    fn enriched_program_plans_recovered_bindings_with_shapes() {
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("function factory() { return 42; }".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let mut binding_shapes = BindingShapeSolution::default();
        for constraint in model.graph().def_use().constraints() {
            binding_shapes.add_constraint(constraint);
        }
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            binding_shapes,
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");

        assert_eq!(plan.files[0].bindings.len(), 1);
        assert_eq!(plan.files[0].bindings[0].original.as_str(), "factory");
        assert_eq!(plan.files[0].bindings[0].shape, BindingShape::Callable);
        assert!(plan.files[0].bindings[0].source_backed);
    }

    #[test]
    fn input_symbol_without_ast_definition_is_planned_as_not_source_backed() {
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("export const real = 1;".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1),
        );
        rows.symbols.push(SymbolInput {
            module_id: ModuleId(1),
            name: "missing".to_string(),
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");

        let missing = plan.files[0]
            .bindings
            .iter()
            .find(|binding| binding.original.as_str() == "missing")
            .expect("input symbol should be planned");
        assert!(!missing.source_backed);
    }

    #[test]
    fn enriched_program_plans_source_backed_ast_exports() {
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("const answer = 42; export { answer };".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");

        assert_eq!(plan.files[0].exports.len(), 1);
        assert_eq!(plan.files[0].exports[0].binding.as_str(), "answer");
        assert!(plan.files[0].exports[0].source_backed);
    }

    #[test]
    fn source_imported_binding_can_back_source_export() {
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("import { answer } from 'pkg'; export { answer };".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");

        assert!(
            plan.files[0]
                .bindings
                .iter()
                .any(|binding| { binding.original.as_str() == "answer" && binding.source_backed })
        );
        assert_eq!(plan.files[0].exports[0].binding.as_str(), "answer");
    }
}
