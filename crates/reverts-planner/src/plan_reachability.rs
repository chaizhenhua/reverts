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

    #[test]
    fn externalized_namespace_passthrough_folds_exclusively_interior_package_modules() {
        // Drop-closure folding: once a package's public entry is externalized,
        // its body is replaced by a whole-namespace passthrough that carries NO
        // relative imports to the package's interior. Interior modules reachable
        // ONLY through that entry therefore fall out of the cli.ts closure and
        // fold away. A sibling package module that a first-party file still
        // imports DIRECTLY is correctly retained — that residual is the boundary
        // case requiring per-binding public-name recovery, not a planner fold,
        // so the fold must not (and cannot soundly) drop it.
        let mut plan = EmitPlan::default();
        let mut cli = PlannedFile::new("cli.ts");
        cli.push_source("import { entry } from './modules/pkg-entry.js';");
        cli.push_source("import { direct } from './modules/pkg-boundary.js';");
        plan.push_file(cli);

        // Externalized entry: namespace passthrough, only a bare package import.
        let mut entry = PlannedFile::new("modules/pkg-entry.ts");
        entry.push_source("import * as ns from 'some-pkg';");
        entry.push_source("function entry() { return ns; }");
        entry.push_source("export { entry };");
        plan.push_file(entry);

        // Interior module: reachable only via the (now bodiless) entry → folds.
        let mut interior = PlannedFile::new("modules/pkg-interior.ts");
        interior.push_source("export const interior = 1;");
        plan.push_file(interior);

        // Boundary module: first-party cli.ts imports it directly → retained.
        let mut boundary = PlannedFile::new("modules/pkg-boundary.ts");
        boundary.push_source("export const direct = 2;");
        plan.push_file(boundary);

        prune_plan_to_cli_reachable(&mut plan);

        let kept = plan
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();
        assert!(kept.contains(&"modules/pkg-entry.ts"));
        assert!(kept.contains(&"modules/pkg-boundary.ts"));
        assert!(
            !kept.contains(&"modules/pkg-interior.ts"),
            "interior module reachable only through an externalized entry must fold, got {kept:?}"
        );
    }
}
