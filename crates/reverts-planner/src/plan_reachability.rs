//! Emit-plan reachability pruning.
//!
//! When the recovered bundle has a CLI entrypoint, files that are not
//! statically reachable from `cli.ts` do not contribute to the executable
//! program. Dropping them reduces total emitted source instead of merely
//! moving code between modules. The pass only follows planner-emitted static
//! relative imports/re-exports; package imports and unknown source strings are
//! ignored rather than guessed.

use crate::EmitPlan;

const CLI_ENTRYPOINT_PATH: &str = "cli.ts";

pub(crate) fn prune_plan_to_cli_reachable(plan: &mut EmitPlan) {
    if !plan
        .files
        .iter()
        .any(|file| file.path == CLI_ENTRYPOINT_PATH)
    {
        return;
    }
    // Forward static-import closure from the CLI entrypoint, computed over the
    // first-class `ModuleInitGraph`'s raw import adjacency (named/bare imports
    // plus `export … from` re-exports, relative specifiers only). Files outside
    // the closure do not contribute to the executable program and are dropped.
    let graph = reverts_graph::ModuleInitGraph::from_emitted_modules(
        plan.files
            .iter()
            .map(|file| (file.path.clone(), file.body.join("\n"))),
    );
    let reachable = graph.import_reachable_from([CLI_ENTRYPOINT_PATH]);
    plan.files.retain(|file| {
        graph
            .index_of(&file.path)
            .is_some_and(|node| reachable.contains(&node))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PlannedFile;

    #[test]
    fn cli_reachability_keeps_only_static_relative_import_closure() {
        let mut plan = EmitPlan::default();
        let mut cli = PlannedFile::new("cli.ts");
        cli.push_source("import { main } from './modules/entrypoint.js';");
        plan.push_file(cli);
        let mut entry = PlannedFile::new("modules/entrypoint.ts");
        entry.push_source("import { value } from './used.js';import './side-effect.js';");
        entry.push_source("export { value } from './used.js';");
        plan.push_file(entry);
        plan.push_file(PlannedFile::new("modules/used.ts"));
        plan.push_file(PlannedFile::new("modules/side-effect.ts"));
        plan.push_file(PlannedFile::new("modules/dead.ts"));

        prune_plan_to_cli_reachable(&mut plan);

        assert_eq!(
            plan.files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec![
                "cli.ts",
                "modules/entrypoint.ts",
                "modules/used.ts",
                "modules/side-effect.ts"
            ]
        );
    }
}
