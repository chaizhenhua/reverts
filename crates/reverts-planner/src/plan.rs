//! Emit plan data structures.
//!
//! These types are intentionally data-only. Readability and boilerplate
//! rewrites are explicit planner passes (for example
//! `crate::finalize_planned_file`) instead of hidden side effects on plan
//! insertion.

use std::collections::BTreeSet;

use reverts_ir::{BindingName, BindingShape};
use reverts_package::PackageResolution;

use crate::compiler_recovery::{CompilerRecoveryDecision, SourceCompilerStrategy};

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
    /// Late, readability-only binding renames. These are applied by the
    /// emitter after all source recovery/lowering is complete but before
    /// final codegen and parse audit, so graph/planner facts stay keyed by
    /// original recovered names.
    pub readability_renames: Vec<PlannedRename>,
    pub body: Vec<String>,
    pub compiler_recovery: CompilerRecoveryDecision,
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
            compiler_recovery: CompilerRecoveryDecision::default(),
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

    pub fn add_readability_rename(&mut self, rename: PlannedRename) {
        self.readability_renames.push(rename);
    }

    pub fn set_compiler_recovery(&mut self, compiler_recovery: CompilerRecoveryDecision) {
        self.compiler_recovery = compiler_recovery;
    }

    #[must_use]
    pub const fn source_strategy(&self) -> SourceCompilerStrategy {
        self.compiler_recovery.strategy
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
