use std::collections::{BTreeMap, BTreeSet};

use reverts_input::{InputBundle, ModuleDependencyTarget, ModuleInput};
use reverts_ir::{BindingConstraint, BindingConstraintKind, BindingName, DefUseGraph, ModuleId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevertsGraph {
    modules: BTreeMap<ModuleId, ModuleInput>,
    definitions: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    def_use: DefUseGraph,
    import_export: ImportExportGraph,
}

impl RevertsGraph {
    #[must_use]
    pub fn from_input(input: &InputBundle) -> Self {
        let modules = input
            .modules
            .iter()
            .map(|module| (module.id, module.clone()))
            .collect::<BTreeMap<_, _>>();

        let mut definitions: BTreeMap<ModuleId, BTreeSet<BindingName>> = BTreeMap::new();
        let mut def_use = DefUseGraph::default();
        for symbol in &input.symbols {
            def_use.define(symbol.module_id, symbol.name.clone());
            definitions
                .entry(symbol.module_id)
                .or_default()
                .insert(BindingName::new(symbol.name.clone()));
        }

        let mut import_export = ImportExportGraph::default();
        for dependency in &input.dependencies {
            match &dependency.target {
                ModuleDependencyTarget::Module(module_id) => {
                    import_export.record_module_import(dependency.from_module_id, *module_id);
                }
                ModuleDependencyTarget::Package { specifier } => {
                    import_export
                        .record_package_import(dependency.from_module_id, specifier.clone());
                }
            }
        }

        Self {
            modules,
            definitions,
            def_use,
            import_export,
        }
    }

    pub fn record_read(&mut self, module_id: ModuleId, binding: impl Into<String>) {
        self.def_use.read(module_id, binding);
    }

    pub fn record_constraint(
        &mut self,
        module_id: ModuleId,
        binding: impl Into<String>,
        kind: BindingConstraintKind,
    ) {
        self.def_use
            .constrain(BindingConstraint::new(module_id, binding, kind));
    }

    #[must_use]
    pub fn module(&self, module_id: ModuleId) -> Option<&ModuleInput> {
        self.modules.get(&module_id)
    }

    #[must_use]
    pub fn definitions_for(&self, module_id: ModuleId) -> Vec<BindingName> {
        self.definitions
            .get(&module_id)
            .map(|bindings| bindings.iter().cloned().collect())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn def_use(&self) -> &DefUseGraph {
        &self.def_use
    }

    #[must_use]
    pub fn import_export(&self) -> &ImportExportGraph {
        &self.import_export
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImportExportGraph {
    module_imports: BTreeMap<ModuleId, BTreeSet<ModuleId>>,
    package_imports: BTreeMap<ModuleId, BTreeSet<String>>,
    exports: BTreeMap<ModuleId, BTreeSet<BindingName>>,
}

impl ImportExportGraph {
    pub fn record_module_import(&mut self, from_module_id: ModuleId, target_module_id: ModuleId) {
        self.module_imports
            .entry(from_module_id)
            .or_default()
            .insert(target_module_id);
    }

    pub fn record_package_import(
        &mut self,
        from_module_id: ModuleId,
        specifier: impl Into<String>,
    ) {
        self.package_imports
            .entry(from_module_id)
            .or_default()
            .insert(specifier.into());
    }

    pub fn record_export(&mut self, module_id: ModuleId, binding: impl Into<String>) {
        self.exports
            .entry(module_id)
            .or_default()
            .insert(BindingName::new(binding));
    }

    #[must_use]
    pub fn package_imports_for(&self, module_id: ModuleId) -> Vec<&str> {
        self.package_imports
            .get(&module_id)
            .map(|imports| imports.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use reverts_input::{InputBundle, InputRows, ModuleInput, ProjectInput, SymbolInput};
    use reverts_ir::ModuleId;

    use super::RevertsGraph;

    #[test]
    fn input_symbols_become_graph_definitions() {
        let mut rows = InputRows::new(ProjectInput {
            id: 1,
            name: "fixture".to_string(),
        });
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "m1", "src/index.ts"));
        rows.symbols.push(SymbolInput {
            module_id: ModuleId(1),
            name: "main".to_string(),
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let graph = RevertsGraph::from_input(&input);

        assert_eq!(graph.definitions_for(ModuleId(1))[0].as_str(), "main");
    }

    #[test]
    fn unresolved_reads_remain_visible_in_def_use_graph() {
        let mut rows = InputRows::new(ProjectInput {
            id: 1,
            name: "fixture".to_string(),
        });
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "m1", "src/index.ts"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let mut graph = RevertsGraph::from_input(&input);

        graph.record_read(ModuleId(1), "missing");

        assert_eq!(graph.def_use().unresolved_reads()[0].1.as_str(), "missing");
    }
}
