//! Emit plan data structures.
//!
//! These types are intentionally data-only. Readability and boilerplate
//! rewrites are explicit planner passes (for example
//! `crate::finalize_planned_file`) instead of hidden side effects on plan
//! insertion.

use std::collections::BTreeSet;

use reverts_ir::{BindingName, BindingShape};
use reverts_package::PackageResolution;

use crate::compiler_preservation::{CompilerPreservationDecision, SourceCompilerStrategy};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EmitPlan {
    pub files: Vec<PlannedFile>,
}

impl EmitPlan {
    pub fn push_file(&mut self, file: PlannedFile) {
        self.files.push(file);
    }

    pub fn validate(self) -> Result<ValidatedEmitPlan, crate::PlanError> {
        let mut paths = BTreeSet::<String>::new();
        let mut files = Vec::with_capacity(self.files.len());
        for file in self.files {
            if !paths.insert(file.path.clone()) {
                return Err(crate::PlanError::InvalidEmitPlan {
                    message: format!("duplicate planned output path '{}'", file.path),
                });
            }
            files.push(ValidatedPlannedFile::try_from(file)?);
        }
        Ok(ValidatedEmitPlan { files })
    }
}

/// Emit plan that has passed planner-owned structural validation.
///
/// The validation is intentionally narrow today: it rejects duplicate output
/// paths and malformed file-level plan records before the emitter can see them.
/// Recoverable semantic findings (for example rejected package imports) remain
/// audit-owned per ADR 0002. New fatal planner-bug invariants should be added
/// here so the emitter accepts a typed, pre-checked plan instead of
/// rediscovering them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedEmitPlan {
    files: Vec<ValidatedPlannedFile>,
}

impl ValidatedEmitPlan {
    pub fn files(&self) -> impl Iterator<Item = &PlannedFile> {
        self.files.iter().map(ValidatedPlannedFile::as_file)
    }

    #[must_use]
    pub fn into_inner(self) -> EmitPlan {
        EmitPlan {
            files: self
                .files
                .into_iter()
                .map(ValidatedPlannedFile::into_inner)
                .collect(),
        }
    }
}

/// File-level typestate produced by [`PlannedFile::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedPlannedFile {
    file: PlannedFile,
}

impl ValidatedPlannedFile {
    #[must_use]
    pub const fn as_file(&self) -> &PlannedFile {
        &self.file
    }

    #[must_use]
    pub fn into_inner(self) -> PlannedFile {
        self.file
    }
}

impl TryFrom<PlannedFile> for ValidatedPlannedFile {
    type Error = crate::PlanError;

    fn try_from(file: PlannedFile) -> Result<Self, Self::Error> {
        if file.path.trim().is_empty() {
            return Err(crate::PlanError::InvalidEmitPlan {
                message: "planned file path is empty".to_string(),
            });
        }
        let mut import_slots = BTreeSet::<(String, Option<String>)>::new();
        for import in &file.imports {
            if import.namespace.as_str().trim().is_empty() {
                return Err(crate::PlanError::InvalidEmitPlan {
                    message: format!("file '{}' has an import with an empty namespace", file.path),
                });
            }
            let slot = (
                import.namespace.as_str().to_string(),
                import.resolution.specifier().map(str::to_string),
            );
            if !import_slots.insert(slot) {
                return Err(crate::PlanError::InvalidEmitPlan {
                    message: format!(
                        "file '{}' has a duplicate planned import for '{}'",
                        file.path, import.namespace
                    ),
                });
            }
        }
        let mut declarations = file
            .imports
            .iter()
            .map(|import| import.namespace.clone())
            .chain(file.bindings.iter().map(|binding| binding.emitted.clone()))
            .collect::<BTreeSet<_>>();
        declarations.extend(file.bindings.iter().map(|binding| binding.original.clone()));

        for binding in &file.bindings {
            if !binding.source_backed {
                return Err(crate::PlanError::InvalidEmitPlan {
                    message: format!(
                        "file '{}' has a synthetic binding '{}' without a declaration owner",
                        file.path, binding.emitted
                    ),
                });
            }
        }

        let mut exports = BTreeSet::<String>::new();
        for export in &file.exports {
            if !exports.insert(export.binding.as_str().to_string()) {
                return Err(crate::PlanError::InvalidEmitPlan {
                    message: format!(
                        "file '{}' has a duplicate planned export for '{}'",
                        file.path, export.binding
                    ),
                });
            }
            if !export.source_backed && !declarations.contains(&export.binding) {
                return Err(crate::PlanError::InvalidEmitPlan {
                    message: format!(
                        "file '{}' has a generated export '{}' without a declaration or import",
                        file.path, export.binding
                    ),
                });
            }
        }
        Ok(Self { file })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    pub path: String,
    pub imports: Vec<PlannedImport>,
    pub bindings: Vec<PlannedBinding>,
    pub exports: Vec<PlannedExport>,
    /// Late, readability-only binding renames. These are applied by the
    /// emitter after all source recovery/lowering is complete but before
    /// final codegen and parse audit, so graph/planner facts stay keyed by
    /// original recovered names.
    pub readability_renames: Vec<PlannedRename>,
    pub body: Vec<String>,
    pub compiler_preservation: CompilerPreservationDecision,
}

impl PlannedFile {
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            imports: Vec::new(),
            bindings: Vec::new(),
            exports: Vec::new(),
            readability_renames: Vec::new(),
            body: Vec::new(),
            compiler_preservation: CompilerPreservationDecision::default(),
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
        // A binding can only be exported once; multiple planner passes may each
        // request the same export (e.g. a source-backed re-export plus an adapter
        // export of the same name). Deduplicate here so the emit plan stays valid
        // instead of failing the "duplicate planned export" invariant. A
        // source-backed request upgrades an existing synthetic export, since the
        // raw body already carries the export statement.
        if let Some(existing) = self
            .exports
            .iter_mut()
            .find(|export| export.binding == binding)
        {
            if source_backed {
                existing.source_backed = true;
            }
            return;
        }
        self.exports.push(PlannedExport {
            binding,
            source_backed,
        });
    }

    pub fn push_source(&mut self, source: impl Into<String>) {
        self.body.push(source.into());
    }

    pub fn add_readability_rename(&mut self, rename: PlannedRename) {
        self.readability_renames.push(rename);
    }

    pub fn set_compiler_preservation(
        &mut self,
        compiler_preservation: CompilerPreservationDecision,
    ) {
        self.compiler_preservation = compiler_preservation;
    }

    #[must_use]
    pub const fn source_strategy(&self) -> SourceCompilerStrategy {
        self.compiler_preservation.strategy
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedImport {
    pub namespace: BindingName,
    pub resolution: PackageResolution,
    pub source_backed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedRename {
    pub original: BindingName,
    pub renamed: BindingName,
}

impl PlannedRename {
    #[must_use]
    pub fn new(original: BindingName, renamed: BindingName) -> Self {
        Self { original, renamed }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedBinding {
    pub original: BindingName,
    pub emitted: BindingName,
    pub shape: BindingShape,
    pub source_backed: bool,
    /// Paper #7 downstream: property names observed on this binding when
    /// shape is `NamespaceObject`. Empty for every other shape and for
    /// namespaces whose members the solver could not see.
    pub known_members: BTreeSet<BindingName>,
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
            known_members: BTreeSet::new(),
        }
    }

    #[must_use]
    pub fn with_known_members(mut self, known_members: BTreeSet<BindingName>) -> Self {
        self.known_members = known_members;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedExport {
    pub binding: BindingName,
    pub source_backed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_export_deduplicates_by_binding() {
        // Multiple passes may request the same export; only one PlannedExport
        // must result, or the emit plan fails the "duplicate planned export"
        // invariant. A later source-backed request upgrades the flag.
        let mut file = PlannedFile::new("modules/a.ts");
        file.add_export_with_source_backed(BindingName::new("Tm1"), false);
        file.add_export_with_source_backed(BindingName::new("Tm1"), true);
        file.add_export_with_source_backed(BindingName::new("Other"), false);

        let names = file
            .exports
            .iter()
            .map(|export| export.binding.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["Tm1", "Other"]);
        assert!(
            file.exports
                .iter()
                .find(|export| export.binding.as_str() == "Tm1")
                .expect("Tm1 export")
                .source_backed,
            "source-backed request should upgrade the existing export"
        );
    }
}
