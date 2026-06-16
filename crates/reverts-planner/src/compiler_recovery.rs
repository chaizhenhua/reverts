//! Compiler-recovery decision types.
//!
//! Every module carries a `ModuleCompilerProfile` from `reverts-model`
//! identifying which compiler shaped its source (webpack runtime,
//! esbuild helpers, rollup facade, etc.). The planner translates that
//! profile into two adjacent decisions:
//!
//! - `SourceCompilerStrategy` decides how to *parse* the module — the
//!   parse goal and an optional path hint.
//! - `CompilerRecoveryAction` decides how to *emit* the module — either
//!   pass-through `DirectModuleSource` or one of the `Preserve…`
//!   actions that stamps a `// reverts-recovery: <compiler>` banner so
//!   the consumer can see the planner punted on full recovery.
//!
//! `CompilerRecoveryDecision` bundles both decisions plus the evidence
//! the model collected, so audit consumers can see *why* the planner
//! made the call without recomputing it.

use reverts_js::ParseGoal;
use reverts_model::{CompilerEvidence, CompilerKind, ModuleCompilerProfile};

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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CompilerRecoveryAction {
    #[default]
    DirectModuleSource,
    PreserveWebpackRuntime,
    PreserveEsbuildHelpers,
    PreserveRollupFacade,
    PreserveBabelTranspiledOutput,
    PreserveTerserMinifiedOutput,
}

impl CompilerRecoveryAction {
    #[must_use]
    pub const fn from_compiler(compiler: CompilerKind) -> Self {
        match compiler {
            CompilerKind::Unknown => Self::DirectModuleSource,
            CompilerKind::Webpack => Self::PreserveWebpackRuntime,
            CompilerKind::Esbuild => Self::PreserveEsbuildHelpers,
            CompilerKind::Rollup => Self::PreserveRollupFacade,
            CompilerKind::Babel => Self::PreserveBabelTranspiledOutput,
            CompilerKind::Terser => Self::PreserveTerserMinifiedOutput,
        }
    }

    /// Short banner text that surfaces the recovery decision in the emitted
    /// source. Returns `None` for `DirectModuleSource` so untransformed user
    /// code stays banner-free.
    #[must_use]
    pub const fn recovery_banner(self) -> Option<&'static str> {
        match self {
            Self::DirectModuleSource => None,
            Self::PreserveWebpackRuntime => Some("reverts-recovery: webpack"),
            Self::PreserveEsbuildHelpers => Some("reverts-recovery: esbuild"),
            Self::PreserveRollupFacade => Some("reverts-recovery: rollup"),
            Self::PreserveBabelTranspiledOutput => Some("reverts-recovery: babel"),
            Self::PreserveTerserMinifiedOutput => Some("reverts-recovery: terser"),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CompilerRecoveryDecision {
    pub strategy: SourceCompilerStrategy,
    pub action: CompilerRecoveryAction,
    pub minified: bool,
    pub evidence: Vec<CompilerEvidence>,
}

impl CompilerRecoveryDecision {
    #[must_use]
    pub fn from_profile(profile: &ModuleCompilerProfile) -> Self {
        Self {
            strategy: SourceCompilerStrategy::from_profile(profile),
            action: CompilerRecoveryAction::from_compiler(profile.compiler),
            minified: profile.minified,
            evidence: profile.evidence.clone(),
        }
    }
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

    #[must_use]
    pub fn path_hint(self, path: &str) -> Option<&std::path::Path> {
        match self {
            Self::DirectSource => Some(std::path::Path::new(path)),
            Self::WebpackRuntime
            | Self::EsbuildHelpers
            | Self::RollupFacade
            | Self::BabelTranspiled
            | Self::TerserMinified => None,
        }
    }
}
