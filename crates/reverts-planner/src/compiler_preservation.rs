//! Compiler-preservation decision types.
//!
//! Every module carries a `ModuleCompilerProfile` from `reverts-model`
//! identifying which compiler shaped its source (webpack runtime,
//! esbuild helpers, rollup facade, etc.). The planner translates that
//! profile into two adjacent decisions:
//!
//! - `SourceCompilerStrategy` decides how to *parse* the module — the
//!   parse goal and an optional path hint.
//! - `CompilerPreservationAction` decides how to *emit* the module — either
//!   pass-through `DirectModuleSource` or one of the `Preserve…`
//!   actions that stamps a `// reverts-compiler-preserved: <compiler>` banner so
//!   the consumer can see that a compiler-shaped source boundary was preserved
//!   explicitly instead of being silently rewritten.
//!
//! `CompilerPreservationDecision` bundles both decisions plus the evidence
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
pub enum CompilerPreservationAction {
    #[default]
    DirectModuleSource,
    PreserveWebpackRuntime,
    PreserveEsbuildHelpers,
    PreserveRollupFacade,
    PreserveBabelTranspiledOutput,
    PreserveTerserMinifiedOutput,
}

impl CompilerPreservationAction {
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

    /// Short banner text that surfaces the preservation decision in the emitted
    /// source. Returns `None` for `DirectModuleSource` so untransformed user
    /// code stays banner-free.
    #[must_use]
    pub const fn preservation_banner(self) -> Option<&'static str> {
        match self {
            Self::DirectModuleSource => None,
            Self::PreserveWebpackRuntime => Some("reverts-compiler-preserved: webpack"),
            Self::PreserveEsbuildHelpers => Some("reverts-compiler-preserved: esbuild"),
            Self::PreserveRollupFacade => Some("reverts-compiler-preserved: rollup"),
            Self::PreserveBabelTranspiledOutput => Some("reverts-compiler-preserved: babel"),
            Self::PreserveTerserMinifiedOutput => Some("reverts-compiler-preserved: terser"),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CompilerPreservationDecision {
    pub strategy: SourceCompilerStrategy,
    pub action: CompilerPreservationAction,
    pub minified: bool,
    pub evidence: Vec<CompilerEvidence>,
}

impl CompilerPreservationDecision {
    #[must_use]
    pub fn from_profile(profile: &ModuleCompilerProfile) -> Self {
        Self {
            strategy: SourceCompilerStrategy::from_profile(profile),
            action: CompilerPreservationAction::from_compiler(profile.compiler),
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
