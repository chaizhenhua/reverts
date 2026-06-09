use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use reverts_ir::{ModuleId, ModuleKind, is_valid_package_name};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInput {
    pub id: u32,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFileInput {
    pub id: u32,
    pub path: String,
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleInput {
    pub id: ModuleId,
    pub kind: ModuleKind,
    pub semantic_path: String,
    pub source_file_id: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolInput {
    pub module_id: ModuleId,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleDependencyInput {
    pub from_module_id: ModuleId,
    pub target: ModuleDependencyTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleDependencyTarget {
    Module(ModuleId),
    Package { specifier: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageAttributionInput {
    pub module_id: ModuleId,
    pub package_name: String,
    pub package_version: Option<String>,
    pub subpath: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputBundle {
    pub project: ProjectInput,
    pub source_files: Vec<SourceFileInput>,
    pub modules: Vec<ModuleInput>,
    pub symbols: Vec<SymbolInput>,
    pub dependencies: Vec<ModuleDependencyInput>,
    pub package_attributions: Vec<PackageAttributionInput>,
}

impl InputBundle {
    pub fn from_rows(rows: InputRows) -> Result<Self, InputBundleError> {
        let module_ids = rows
            .modules
            .iter()
            .map(|module| module.id)
            .collect::<BTreeSet<_>>();

        for symbol in &rows.symbols {
            ensure_module_exists(symbol.module_id, &module_ids, "symbol")?;
        }

        for dependency in &rows.dependencies {
            ensure_module_exists(dependency.from_module_id, &module_ids, "dependency")?;
            if let ModuleDependencyTarget::Module(target_module_id) = dependency.target {
                ensure_module_exists(target_module_id, &module_ids, "dependency target")?;
            }
        }

        for attribution in &rows.package_attributions {
            ensure_module_exists(attribution.module_id, &module_ids, "package attribution")?;
            if !is_valid_package_name(&attribution.package_name) {
                return Err(InputBundleError::InvalidPackageName(
                    attribution.package_name.clone(),
                ));
            }
        }

        Ok(Self {
            project: rows.project,
            source_files: rows.source_files,
            modules: rows.modules,
            symbols: rows.symbols,
            dependencies: rows.dependencies,
            package_attributions: rows.package_attributions,
        })
    }

    #[must_use]
    pub fn module_ids(&self) -> BTreeSet<ModuleId> {
        self.modules.iter().map(|module| module.id).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputRows {
    pub project: ProjectInput,
    pub source_files: Vec<SourceFileInput>,
    pub modules: Vec<ModuleInput>,
    pub symbols: Vec<SymbolInput>,
    pub dependencies: Vec<ModuleDependencyInput>,
    pub package_attributions: Vec<PackageAttributionInput>,
}

impl InputRows {
    #[must_use]
    pub fn new(project: ProjectInput) -> Self {
        Self {
            project,
            source_files: Vec::new(),
            modules: Vec::new(),
            symbols: Vec::new(),
            dependencies: Vec::new(),
            package_attributions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputBundleError {
    UnknownModule {
        module_id: ModuleId,
        owner: &'static str,
    },
    InvalidPackageName(String),
}

impl fmt::Display for InputBundleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownModule { module_id, owner } => {
                write!(
                    formatter,
                    "{owner} references unknown module {}",
                    module_id.0
                )
            }
            Self::InvalidPackageName(package_name) => {
                write!(formatter, "invalid package name {package_name}")
            }
        }
    }
}

impl Error for InputBundleError {}

fn ensure_module_exists(
    module_id: ModuleId,
    module_ids: &BTreeSet<ModuleId>,
    owner: &'static str,
) -> Result<(), InputBundleError> {
    if module_ids.contains(&module_id) {
        Ok(())
    } else {
        Err(InputBundleError::UnknownModule { module_id, owner })
    }
}

#[cfg(test)]
mod tests {
    use reverts_ir::{ModuleId, ModuleKind};

    use super::{
        InputBundle, InputBundleError, InputRows, ModuleInput, PackageAttributionInput,
        ProjectInput, SymbolInput,
    };

    #[test]
    fn rows_build_a_self_contained_input_bundle() {
        let mut rows = InputRows::new(ProjectInput {
            id: 1,
            name: "fixture".to_string(),
        });
        rows.modules.push(ModuleInput {
            id: ModuleId(10),
            kind: ModuleKind::Application,
            semantic_path: "src/index.ts".to_string(),
            source_file_id: None,
        });
        rows.symbols.push(SymbolInput {
            module_id: ModuleId(10),
            name: "main".to_string(),
        });

        let bundle = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        assert_eq!(bundle.project.name, "fixture");
        assert!(bundle.module_ids().contains(&ModuleId(10)));
    }

    #[test]
    fn invalid_package_attribution_is_rejected_before_planning() {
        let mut rows = InputRows::new(ProjectInput {
            id: 1,
            name: "fixture".to_string(),
        });
        rows.modules.push(ModuleInput {
            id: ModuleId(10),
            kind: ModuleKind::Package,
            semantic_path: "node_modules/@smithy/XY7/index.js".to_string(),
            source_file_id: None,
        });
        rows.package_attributions.push(PackageAttributionInput {
            module_id: ModuleId(10),
            package_name: "@smithy/XY7".to_string(),
            package_version: None,
            subpath: None,
        });

        let error = InputBundle::from_rows(rows);

        assert!(matches!(
            error,
            Err(InputBundleError::InvalidPackageName(package_name))
                if package_name == "@smithy/XY7"
        ));
    }
}
