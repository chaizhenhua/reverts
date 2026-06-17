//! Emit-plan reachability pruning.
//!
//! When the recovered bundle has a CLI entrypoint, files that are not
//! statically reachable from `cli.ts` do not contribute to the executable
//! program. Dropping them reduces total emitted source instead of merely
//! moving code between modules. The pass only follows planner-emitted static
//! relative imports/re-exports; package imports and unknown source strings are
//! ignored rather than guessed.

use std::collections::{BTreeMap, BTreeSet};

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
    let path_set = plan
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect::<BTreeSet<_>>();
    let imports_by_path = plan
        .files
        .iter()
        .map(|file| {
            (
                file.path.clone(),
                reachable_relative_import_paths(file.path.as_str(), &file.body, &path_set),
            )
        })
        .collect::<BTreeMap<_, _>>();

    let mut reachable = BTreeSet::<String>::new();
    let mut queue = vec![CLI_ENTRYPOINT_PATH.to_string()];
    while let Some(path) = queue.pop() {
        if !reachable.insert(path.clone()) {
            continue;
        }
        if let Some(imports) = imports_by_path.get(&path) {
            queue.extend(
                imports
                    .iter()
                    .filter(|import| !reachable.contains(*import))
                    .cloned(),
            );
        }
    }

    plan.files.retain(|file| reachable.contains(&file.path));
}

fn reachable_relative_import_paths(
    file_path: &str,
    body: &[String],
    path_set: &BTreeSet<String>,
) -> BTreeSet<String> {
    body.iter()
        .flat_map(|source| static_module_specifiers(source))
        .filter(|specifier| specifier.starts_with("./") || specifier.starts_with("../"))
        .filter_map(|specifier| resolve_relative_plan_path(file_path, specifier.as_str()))
        .filter(|path| path_set.contains(path))
        .collect()
}

fn static_module_specifiers(source: &str) -> Vec<String> {
    source
        .split(';')
        .filter_map(|statement| static_module_specifier_from_statement(statement.trim()))
        .collect()
}

fn static_module_specifier_from_statement(statement: &str) -> Option<String> {
    if let Some(rest) = statement.strip_prefix("import '") {
        return rest.strip_suffix('\'').map(str::to_string);
    }
    if statement.starts_with("import ") || statement.starts_with("export ") {
        let (_head, rest) = statement.rsplit_once(" from '")?;
        return rest.strip_suffix('\'').map(str::to_string);
    }
    None
}

fn resolve_relative_plan_path(file_path: &str, specifier: &str) -> Option<String> {
    let directory = file_path
        .rsplit_once('/')
        .map_or("", |(directory, _)| directory);
    let mut parts = if directory.is_empty() {
        Vec::<&str>::new()
    } else {
        directory.split('/').collect::<Vec<_>>()
    };
    for part in specifier.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    let mut path = parts.join("/");
    if let Some(stripped) = path.strip_suffix(".js") {
        path = format!("{stripped}.ts");
    }
    Some(path)
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
