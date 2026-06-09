use reverts_graph::RevertsGraph;
use reverts_ir::{BindingName, BindingShape, ModuleId, ModuleKind};
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
    pub declarations: Vec<PlannedDeclaration>,
    pub exports: Vec<PlannedExport>,
    pub body: Vec<String>,
}

impl PlannedFile {
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            imports: Vec::new(),
            declarations: Vec::new(),
            exports: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn declare(&mut self, binding: BindingName, shape: BindingShape) {
        self.declarations
            .push(PlannedDeclaration { binding, shape });
    }

    pub fn add_import(&mut self, import: PlannedImport) {
        self.imports.push(import);
    }

    pub fn add_export(&mut self, binding: BindingName) {
        self.exports.push(PlannedExport { binding });
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedImport {
    pub namespace: BindingName,
    pub resolution: PackageResolution,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedDeclaration {
    pub binding: BindingName,
    pub shape: BindingShape,
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

            for original_binding in program.model().graph().definitions_for(module.id) {
                let binding = program
                    .semantic_names()
                    .binding_name(module.id, original_binding.as_str())
                    .cloned()
                    .unwrap_or_else(|| BindingName::new(original_binding.as_str()));
                let shape = match program.binding_shape(module.id, original_binding.as_str()) {
                    BindingShape::Unknown => BindingShape::Value,
                    shape => shape,
                };
                file.declare(binding.clone(), shape);
                file.add_export(binding);
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
            file.declare(binding, BindingShape::Value);
        }
        file
    }

    #[must_use]
    pub fn plan_synthetic_file(
        self,
        path: impl Into<String>,
        bindings: impl IntoIterator<Item = SyntheticBindingUse>,
    ) -> PlannedFile {
        let mut file = PlannedFile::new(path);
        for binding in bindings {
            file.declare(binding.binding, binding.shape);
        }
        file
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntheticBindingUse {
    pub binding: BindingName,
    pub shape: BindingShape,
}

impl SyntheticBindingUse {
    #[must_use]
    pub fn new(binding: impl Into<String>, shape: BindingShape) -> Self {
        Self {
            binding: BindingName::new(binding),
            shape,
        }
    }
}

#[cfg(test)]
mod tests {
    use reverts_ir::BindingShape;

    use super::{ImportExportPlanner, SyntheticBindingUse};

    #[test]
    fn synthetic_usage_is_planned_with_a_declaration() {
        let planner = ImportExportPlanner;

        let file = planner.plan_synthetic_file(
            "src/index.ts",
            [SyntheticBindingUse::new(
                "__reverts_ns_pkg",
                BindingShape::NamespaceObject,
            )],
        );

        assert_eq!(file.declarations.len(), 1);
        assert_eq!(file.declarations[0].binding.as_str(), "__reverts_ns_pkg");
    }
}
