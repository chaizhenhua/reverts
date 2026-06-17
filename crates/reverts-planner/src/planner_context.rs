//! Planner-wide context and pass plumbing.
//!
//! This is the first step toward a pass-oriented planner architecture: the
//! public planner remains a small facade, while passes receive one shared
//! context instead of rebuilding analysis indexes ad hoc.

use reverts_model::EnrichedProgram;

use crate::{PlanError, PlannerAnalysis};

/// Immutable planning context shared by planner passes.
#[derive(Debug)]
pub(crate) struct PlannerContext<'a> {
    program: &'a EnrichedProgram,
    analysis: PlannerAnalysis,
}

impl<'a> PlannerContext<'a> {
    #[must_use]
    pub(crate) fn new(program: &'a EnrichedProgram) -> Self {
        Self {
            program,
            analysis: PlannerAnalysis::from_program(program),
        }
    }

    #[must_use]
    pub(crate) const fn program(&self) -> &'a EnrichedProgram {
        self.program
    }

    #[must_use]
    pub(crate) const fn analysis(&self) -> &PlannerAnalysis {
        &self.analysis
    }
}

/// A named planner phase. New planner subsystems should be introduced as
/// passes before their internals are split further; this keeps ordering and
/// ownership visible at the facade.
pub(crate) trait PlannerPass {
    fn run(&self, context: &PlannerContext<'_>) -> Result<(), PlanError>;
}

/// Analysis construction is currently the first pass. It is deliberately tiny:
/// calling it through the trait makes the facade exercise the same pass hook
/// future mutating passes will use without changing planner output yet.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct AnalysisReadyPass;

impl PlannerPass for AnalysisReadyPass {
    fn run(&self, context: &PlannerContext<'_>) -> Result<(), PlanError> {
        let _ = context.analysis();
        Ok(())
    }
}
