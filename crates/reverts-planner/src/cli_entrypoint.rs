//! Synthesize `cli.ts` when the bundle has a runtime entrypoint.
//!
//! Every bundled application has at most one runtime entrypoint —
//! a top-level call recorded in the runtime prelude (see
//! `RuntimeEntrypoint`). When present, the planner emits a tiny
//! `cli.ts` shim that imports the entrypoint binding from its runtime
//! helper file and awaits it, plus a `#!/usr/bin/env node` shebang so
//! the file is directly executable.

use std::collections::BTreeSet;

use reverts_model::EnrichedProgram;

use crate::relative_paths::relative_import_specifier;
use crate::statements::{named_import_statement, runtime_helpers_path};
use crate::{EmitPlan, PlannedFile, runtime_entrypoint};

pub(crate) fn emit_cli_entrypoint(program: &EnrichedProgram, plan: &mut EmitPlan) {
    let Some((_prelude, entrypoint)) = runtime_entrypoint(program) else {
        return;
    };
    let mut file = PlannedFile::new("cli.ts");
    file.push_source("#!/usr/bin/env node");
    let helper_path = runtime_helpers_path(entrypoint.source_file_id);
    let specifier = relative_import_specifier("cli.ts", helper_path.as_str());
    let entrypoint_imports = BTreeSet::from([entrypoint.callee.clone()]);
    file.push_source(named_import_statement(
        entrypoint_imports.iter(),
        specifier.as_str(),
    ));
    file.push_source(format!("await {}();", entrypoint.callee.as_str()));
    plan.push_file(file);
}
