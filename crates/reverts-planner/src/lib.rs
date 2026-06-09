use reverts_graph::RevertsGraph;
use reverts_input::InputBundle;
use reverts_ir::{BindingName, ModuleId, ModuleKind};
use reverts_model::EnrichedProgram;
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
    pub exports: Vec<PlannedExport>,
    pub body: Vec<String>,
}

impl PlannedFile {
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            imports: Vec::new(),
            exports: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn add_import(&mut self, import: PlannedImport) {
        self.imports.push(import);
    }

    pub fn add_export(&mut self, binding: BindingName) {
        self.exports.push(PlannedExport { binding });
    }

    pub fn push_source(&mut self, source: impl Into<String>) {
        self.body.push(source.into());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedImport {
    pub namespace: BindingName,
    pub resolution: PackageResolution,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedExport {
    pub binding: BindingName,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ImportExportPlanner;

impl ImportExportPlanner {
    #[must_use]
    pub fn plan_enriched_program(self, program: &EnrichedProgram) -> EmitPlan {
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

            for decision in program.package_imports_for(module.id) {
                file.add_import(PlannedImport {
                    namespace: decision.namespace_binding.clone(),
                    resolution: decision.resolution.clone(),
                });
            }

            if let Some(source) = module_source(program.model().input(), module.id) {
                file.push_source(source);
            }

            plan.push_file(file);
        }

        plan
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

fn module_source(input: &InputBundle, module_id: ModuleId) -> Option<&str> {
    let module = input.modules.iter().find(|module| module.id == module_id)?;
    let source_file_id = module.source_file_id?;
    let source = input
        .source_files
        .iter()
        .find(|source_file| source_file.id == source_file_id)?
        .source
        .as_deref()?;

    if let Some(span) = module.source_span {
        return source.get(span.byte_start as usize..span.byte_end as usize);
    }

    let module_count_for_source = input
        .modules
        .iter()
        .filter(|candidate| candidate.source_file_id == Some(source_file_id))
        .count();
    if module_count_for_source != 1 {
        return None;
    }

    Some(source)
}

#[cfg(test)]
mod tests {
    use reverts_input::{InputBundle, InputRows, ModuleInput, ProjectInput, SourceFileInput};
    use reverts_ir::ModuleId;
    use reverts_model::ProgramModel;

    use super::ImportExportPlanner;

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

        let plan = planner.plan_enriched_program(&enriched);

        assert_eq!(plan.files[0].body[0], "export const answer = 42;");
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

        let plan = planner.plan_enriched_program(&enriched);

        assert_eq!(plan.files[0].body[0], "export const one = 1;");
        assert_eq!(plan.files[1].body[0], "export const two = 2;");
    }
}
