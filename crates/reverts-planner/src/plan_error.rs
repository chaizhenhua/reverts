//! Errors produced by `ImportExportPlanner::plan`.
//!
//! `PlanError` is the planner's fatal-failure surface: the operation
//! couldn't produce an emit plan at all. Recoverable issues — missing
//! source bodies, unresolvable bare imports, asset gaps — flow through
//! `AuditReport` instead (ADR 0002: faithful, not corrective). Two
//! conditions still bubble up here:
//!
//! - `UnparseableSource` — a module's source slice failed normalization
//!   (typically when OXC rejects the recovered text); without a
//!   parseable AST the planner cannot synthesize any plan body.
//! - `UnresolvedRuntimeHelperReferences` — the runtime helpers file
//!   ended up referencing bindings the planner couldn't import or
//!   declare. This indicates a planner bug, not user input, so it
//!   surfaces as an error rather than an audit finding.

use std::error::Error;
use std::fmt;

use reverts_ir::{BindingName, ModuleId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    UnparseableSource {
        module_id: ModuleId,
        path: String,
        message: String,
    },
    UnresolvedRuntimeHelperReferences {
        path: String,
        bindings: Vec<BindingName>,
    },
    InvalidEmitPlan {
        message: String,
    },
}

impl fmt::Display for PlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnparseableSource {
                module_id,
                path,
                message,
            } => write!(
                formatter,
                "module {} source {} failed normalization: {message}",
                module_id.0, path
            ),
            Self::UnresolvedRuntimeHelperReferences { path, bindings } => {
                let bindings = bindings
                    .iter()
                    .map(BindingName::as_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    formatter,
                    "runtime helper {path} has unresolved references: {bindings}"
                )
            }
            Self::InvalidEmitPlan { message } => write!(formatter, "invalid emit plan: {message}"),
        }
    }
}

impl Error for PlanError {}
