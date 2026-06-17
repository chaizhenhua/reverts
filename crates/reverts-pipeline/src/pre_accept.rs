//! Explicit pre-accept transform stage.
//!
//! These transforms run after AST-backed emission and before acceptance audits.
//! They are not post-write repair: each pass has a named, planner-visible
//! purpose and the resulting project is still unaudited until the pipeline runs
//! parse/synthesis checks.

use std::collections::BTreeMap;
use std::ops::Deref;

use reverts_emitter::EmittedProject;
use reverts_input::InputBundle;
use reverts_ir::ModuleId;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreAcceptTransformReport {
    pub transforms: Vec<&'static str>,
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
    let passes: [&dyn PreAcceptTransform; 3] = [
        &CanonicalizeSourceLocations,
        &RewriteAssetReferences,
        &FoldStaticTemplateLiterals,
    ];
    let mut transforms = Vec::with_capacity(passes.len());
    for pass in passes {
        pass.apply(&mut project, context);
        transforms.push(pass.name());
    }
    PreAcceptProject {
        project,
        report: PreAcceptTransformReport { transforms },
    }
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

struct FoldStaticTemplateLiterals;

impl PreAcceptTransform for FoldStaticTemplateLiterals {
    fn name(&self) -> &'static str {
        "fold_static_template_literals"
    }

    fn apply(&self, project: &mut EmittedProject, _context: &PreAcceptContext<'_>) {
        fold_multiline_static_template_literals(project);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CanonicalizeSourceLocations, FoldStaticTemplateLiterals, PreAcceptTransform,
        RewriteAssetReferences,
    };

    #[test]
    fn pre_accept_transform_order_is_explicit() {
        let passes: [&dyn PreAcceptTransform; 3] = [
            &CanonicalizeSourceLocations,
            &RewriteAssetReferences,
            &FoldStaticTemplateLiterals,
        ];
        let names = passes.iter().map(|pass| pass.name()).collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "canonicalize_source_locations",
                "rewrite_asset_references",
                "fold_static_template_literals",
            ]
        );
    }
}
