use std::collections::BTreeMap;

use reverts_graph::{AstWrapperKind, RevertsGraph};
use reverts_input::{InputBundle, ModuleInput, SymbolInput};
use reverts_ir::{BindingName, BindingShape, BindingShapeSolution, ModuleId};
use reverts_package::PackageResolution;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramModel {
    input: InputBundle,
    graph: RevertsGraph,
}

impl ProgramModel {
    #[must_use]
    pub fn from_input(input: InputBundle) -> Self {
        let graph = RevertsGraph::from_input(&input);
        Self { input, graph }
    }

    #[must_use]
    pub fn input(&self) -> &InputBundle {
        &self.input
    }

    #[must_use]
    pub fn graph(&self) -> &RevertsGraph {
        &self.graph
    }

    #[must_use]
    pub fn modules(&self) -> &[ModuleInput] {
        &self.input.modules
    }

    #[must_use]
    pub fn symbols(&self) -> &[SymbolInput] {
        &self.input.symbols
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SemanticNameMap {
    module_paths: BTreeMap<ModuleId, String>,
    bindings: BTreeMap<(ModuleId, BindingName), BindingName>,
}

impl SemanticNameMap {
    pub fn insert_module_path(&mut self, module_id: ModuleId, path: impl Into<String>) {
        self.module_paths.insert(module_id, path.into());
    }

    pub fn insert_binding(
        &mut self,
        module_id: ModuleId,
        original: impl Into<String>,
        semantic: impl Into<String>,
    ) {
        self.bindings.insert(
            (module_id, BindingName::new(original)),
            BindingName::new(semantic),
        );
    }

    #[must_use]
    pub fn module_path(&self, module_id: ModuleId) -> Option<&str> {
        self.module_paths.get(&module_id).map(String::as_str)
    }

    #[must_use]
    pub fn binding_name(&self, module_id: ModuleId, original: &str) -> Option<&BindingName> {
        self.bindings
            .get(&(module_id, BindingName::new(original.to_string())))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageImportDecision {
    pub from_module_id: ModuleId,
    pub namespace_binding: BindingName,
    pub resolution: PackageResolution,
    pub source_backed: bool,
}

impl PackageImportDecision {
    #[must_use]
    pub fn new(
        from_module_id: ModuleId,
        namespace_binding: BindingName,
        resolution: PackageResolution,
    ) -> Self {
        Self::with_source_backed(from_module_id, namespace_binding, resolution, false)
    }

    #[must_use]
    pub fn with_source_backed(
        from_module_id: ModuleId,
        namespace_binding: BindingName,
        resolution: PackageResolution,
        source_backed: bool,
    ) -> Self {
        Self {
            from_module_id,
            namespace_binding,
            resolution,
            source_backed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrichedProgram {
    model: ProgramModel,
    semantic_names: SemanticNameMap,
    package_imports: Vec<PackageImportDecision>,
    binding_shapes: BindingShapeSolution,
    compiler_profile: CompilerProfile,
}

impl EnrichedProgram {
    #[must_use]
    pub fn new(
        model: ProgramModel,
        semantic_names: SemanticNameMap,
        package_imports: Vec<PackageImportDecision>,
        binding_shapes: BindingShapeSolution,
    ) -> Self {
        Self {
            model,
            semantic_names,
            package_imports,
            binding_shapes,
            compiler_profile: CompilerProfile::default(),
        }
    }

    #[must_use]
    pub fn with_compiler_profile(mut self, compiler_profile: CompilerProfile) -> Self {
        self.compiler_profile = compiler_profile;
        self
    }

    #[must_use]
    pub fn model(&self) -> &ProgramModel {
        &self.model
    }

    #[must_use]
    pub fn semantic_names(&self) -> &SemanticNameMap {
        &self.semantic_names
    }

    #[must_use]
    pub fn package_imports(&self) -> &[PackageImportDecision] {
        &self.package_imports
    }

    #[must_use]
    pub fn package_imports_for(&self, module_id: ModuleId) -> Vec<&PackageImportDecision> {
        self.package_imports
            .iter()
            .filter(|decision| decision.from_module_id == module_id)
            .collect()
    }

    #[must_use]
    pub fn binding_shape(&self, module_id: ModuleId, original_name: &str) -> BindingShape {
        self.binding_shapes.shape_of(module_id, original_name)
    }

    #[must_use]
    pub fn compiler_profile(&self) -> &CompilerProfile {
        &self.compiler_profile
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CompilerProfile {
    modules: BTreeMap<ModuleId, ModuleCompilerProfile>,
}

impl CompilerProfile {
    pub fn insert_module(&mut self, module_id: ModuleId, profile: ModuleCompilerProfile) {
        self.modules.insert(module_id, profile);
    }

    #[must_use]
    pub fn module(&self, module_id: ModuleId) -> ModuleCompilerProfile {
        self.modules.get(&module_id).cloned().unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleCompilerProfile {
    pub compiler: CompilerKind,
    pub minified: bool,
    pub evidence: Vec<CompilerEvidence>,
}

impl Default for ModuleCompilerProfile {
    fn default() -> Self {
        Self {
            compiler: CompilerKind::Unknown,
            minified: false,
            evidence: Vec::new(),
        }
    }
}

impl ModuleCompilerProfile {
    #[must_use]
    pub fn new(compiler: CompilerKind, minified: bool, evidence: Vec<CompilerEvidence>) -> Self {
        Self {
            compiler,
            minified,
            evidence,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CompilerKind {
    #[default]
    Unknown,
    Webpack,
    Esbuild,
    Rollup,
    Babel,
    Terser,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompilerEvidence {
    Identifier(String),
    SourcePattern(&'static str),
    MinifiedLayout,
    TopLevelIife(AstWrapperKind),
}

#[cfg(test)]
mod tests {
    use reverts_input::{InputBundle, InputRows, ModuleInput, ProjectInput, SymbolInput};
    use reverts_ir::ModuleId;

    use super::ProgramModel;

    #[test]
    fn program_model_builds_graph_from_input() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "m1", "src/index.ts"));
        rows.symbols.push(SymbolInput::new(ModuleId(1), "main"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let model = ProgramModel::from_input(input);

        assert_eq!(model.modules().len(), 1);
        assert_eq!(
            model.graph().definitions_for(ModuleId(1))[0].as_str(),
            "main"
        );
    }
}
