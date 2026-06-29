//! Collapse the entrypoint-island re-export hub (`modules/entrypoint.ts`).
//!
//! After the chain-split drains the eager island into per-cluster files, the hub
//! is left as a star-topology barrel: it imports every drained binding back and
//! re-exports it, and ~hundreds of consumer files import island bindings THROUGH
//! the hub (`consumer → hub → owner`). This pass repoints each consumer straight
//! at the file that owns the binding (`consumer → owner`); the hub's re-export
//! wall then collapses to what genuinely remains hub-resident, because the
//! emitter only synthesises a hub re-export/back-import for a binding some
//! consumer still imports from the hub.
//!
//! Routing is a planner (decision-layer) responsibility — the emitter renders a
//! pre-decided plan (ADR 0002/0005), so the reroute lives here, not there. See
//! `docs/barrel-direct-routing-plan.md`.

use std::collections::BTreeMap;

use reverts_ir::BindingName;

use crate::EmitPlan;
use crate::cli_entrypoint::ENTRYPOINT_ISLAND_PATH;
use crate::identifiers::is_identifier_like;
use crate::relative_paths::relative_import_specifier;
use crate::statement_parsers::{NamedImportSpecifier, parse_generated_named_import_specifiers};
use crate::statements::named_import_alias_statement;

/// Read-model projecting the plan to `exported binding name → unique owner file`.
///
/// A file's export surface is represented two ways at this stage — structured
/// [`crate::plan::PlannedExport`]s (the planner's own unaliased emissions) and
/// plain `export { local as Wire };` statements the completion passes append to
/// the body text — so the index reads BOTH and normalises each entry to the
/// EXPORTED name (the name a consumer imports), which is what the hub re-exports
/// transparently. This is the single seam the reroute consumes; converging the
/// completion passes onto structured exports later only shrinks what the index
/// reads, never its callers.
///
/// The hub itself and eager-ordered chunk files are excluded; a name exported by
/// more than one eligible file is ambiguous and yields no owner (so it is never
/// rerouted).
pub(crate) struct BindingOwnerIndex {
    owner: BTreeMap<BindingName, String>,
}

impl BindingOwnerIndex {
    pub(crate) fn from_plan(plan: &EmitPlan, hub_path: &str) -> Self {
        let mut accumulator = BTreeMap::<BindingName, Option<String>>::new();
        for file in &plan.files {
            if file.path == hub_path || file.eager_ordered_chunk {
                continue;
            }
            for export in &file.exports {
                record_owner(&mut accumulator, export.binding.clone(), file.path.as_str());
            }
            for source in &file.body {
                for line in source.lines() {
                    for name in exported_names_in_plain_export(line) {
                        record_owner(&mut accumulator, name, file.path.as_str());
                    }
                }
            }
        }
        let owner = accumulator
            .into_iter()
            .filter_map(|(binding, path)| path.map(|path| (binding, path)))
            .collect();
        Self { owner }
    }

    pub(crate) fn owner(&self, binding: &BindingName) -> Option<&str> {
        self.owner.get(binding).map(String::as_str)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.owner.is_empty()
    }
}

fn record_owner(
    accumulator: &mut BTreeMap<BindingName, Option<String>>,
    binding: BindingName,
    path: &str,
) {
    accumulator
        .entry(binding)
        .and_modify(|current| {
            if current.as_deref() != Some(path) {
                *current = None;
            }
        })
        .or_insert_with(|| Some(path.to_string()));
}

/// The exported names of a plain `export { a, b as c };` statement (→ `[a, c]`).
/// Returns empty for a re-export (`export { … } from '…'`) — the file does not
/// own those — and for anything that is not this exact statement shape.
fn exported_names_in_plain_export(line: &str) -> Vec<BindingName> {
    let Some(inner) = line
        .trim()
        .strip_prefix("export { ")
        .and_then(|rest| rest.strip_suffix(" };"))
    else {
        return Vec::new();
    };
    if inner.contains(" from ") {
        return Vec::new();
    }
    inner
        .split(',')
        .filter_map(|raw| {
            let name = raw.trim();
            // The exported name is the segment after `as` (or the whole name).
            let exported = name.rsplit(" as ").next().unwrap_or(name).trim();
            is_identifier_like(exported).then(|| BindingName::new(exported))
        })
        .collect()
}

/// Repoint consumer imports of the entrypoint-island hub at each binding's owner.
///
/// Runs after the import/export-completion passes (final settled graph). Every
/// rerouted binding is one the consumer already imports externally from the
/// (main-bundle) hub, so pointing it at its real owner re-internalises nothing
/// into the consumer's bundle — no bundle-scope guard is required. The consumer
/// imports the hub's re-export name, which equals the owner's export name (the hub
/// re-exports transparently), so a rerouted import keeps its specifier and only
/// changes the source module.
pub(crate) fn reroute_entrypoint_island_barrel(plan: &mut EmitPlan) {
    let hub = ENTRYPOINT_ISLAND_PATH;
    if !plan.files.iter().any(|file| file.path == hub) {
        return;
    }
    let index = BindingOwnerIndex::from_plan(plan, hub);
    if index.is_empty() {
        return;
    }

    for file in &mut plan.files {
        if file.path == hub {
            continue;
        }
        let hub_specifier = relative_import_specifier(file.path.as_str(), hub);
        for source in &mut file.body {
            if !source.contains(hub_specifier.as_str()) {
                continue;
            }
            *source =
                reroute_body_entry(source, file.path.as_str(), hub_specifier.as_str(), &index);
        }
    }
}

/// Rewrite one (possibly multi-line) body entry: any line that is a named import
/// from the hub has its owned specifiers repointed at their owners; every other
/// line is kept verbatim.
fn reroute_body_entry(
    source: &str,
    file_path: &str,
    hub_specifier: &str,
    index: &BindingOwnerIndex,
) -> String {
    let mut out_lines = Vec::with_capacity(source.lines().count());
    for line in source.lines() {
        let Some((specifiers, specifier)) = parse_generated_named_import_specifiers(line.trim())
        else {
            out_lines.push(line.to_string());
            continue;
        };
        if specifier != hub_specifier {
            out_lines.push(line.to_string());
            continue;
        }
        let mut rerouted = BTreeMap::<String, Vec<NamedImportSpecifier>>::new();
        let mut remaining = Vec::<NamedImportSpecifier>::new();
        for specifier in specifiers {
            match index.owner(&specifier.imported) {
                Some(owner_path) if owner_path != file_path => {
                    rerouted
                        .entry(owner_path.to_string())
                        .or_default()
                        .push(specifier);
                }
                _ => remaining.push(specifier),
            }
        }
        if rerouted.is_empty() {
            out_lines.push(line.to_string());
            continue;
        }
        if !remaining.is_empty() {
            out_lines.push(named_import_alias_statement(
                remaining
                    .iter()
                    .map(|specifier| (specifier.imported.as_str(), &specifier.local)),
                hub_specifier,
            ));
        }
        for (owner_path, specifiers) in &rerouted {
            let owner_specifier = relative_import_specifier(file_path, owner_path);
            out_lines.push(named_import_alias_statement(
                specifiers
                    .iter()
                    .map(|specifier| (specifier.imported.as_str(), &specifier.local)),
                owner_specifier.as_str(),
            ));
        }
    }
    out_lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PlannedBinding, PlannedFile};
    use reverts_ir::BindingShape;

    const HUB: &str = ENTRYPOINT_ISLAND_PATH;

    fn file_with_export(path: &str, exported: &str) -> PlannedFile {
        let mut file = PlannedFile::new(path);
        file.add_binding(PlannedBinding::new(
            BindingName::new(exported),
            BindingName::new(exported),
            BindingShape::Unknown,
            true,
        ));
        file.add_export_with_source_backed(BindingName::new(exported), true);
        file
    }

    fn consumer_importing_from_hub(path: &str, import_line: &str) -> PlannedFile {
        let mut file = PlannedFile::new(path);
        file.push_source(import_line);
        file
    }

    fn body_of<'a>(plan: &'a EmitPlan, path: &str) -> &'a str {
        plan.files
            .iter()
            .find(|file| file.path == path)
            .expect("file")
            .body
            .first()
            .map(String::as_str)
            .unwrap_or("")
    }

    #[test]
    fn index_reads_structured_exports() {
        let mut plan = EmitPlan::default();
        plan.push_file(file_with_export("modules/island/auth/oauth.ts", "Wa"));
        let index = BindingOwnerIndex::from_plan(&plan, HUB);
        assert_eq!(
            index.owner(&BindingName::new("Wa")),
            Some("modules/island/auth/oauth.ts")
        );
    }

    #[test]
    fn index_reads_aliased_body_text_exports() {
        let mut plan = EmitPlan::default();
        let mut cluster = PlannedFile::new("modules/island/mcp/zod.ts");
        // The completion passes append aliased plain exports as body text.
        cluster.push_source("export { localImpl as BjA, plain };");
        plan.push_file(cluster);
        let index = BindingOwnerIndex::from_plan(&plan, HUB);
        assert_eq!(
            index.owner(&BindingName::new("BjA")),
            Some("modules/island/mcp/zod.ts"),
            "the EXPORTED name (after `as`) is the owner key"
        );
        assert_eq!(
            index.owner(&BindingName::new("plain")),
            Some("modules/island/mcp/zod.ts")
        );
        assert_eq!(
            index.owner(&BindingName::new("localImpl")),
            None,
            "the local name is not what a consumer imports"
        );
    }

    #[test]
    fn index_ignores_reexport_from_and_dedupes_ambiguous() {
        let mut plan = EmitPlan::default();
        // A re-export `from` does not make this file an owner.
        let mut barrel = PlannedFile::new("modules/island/barrel.ts");
        barrel.push_source("export { X } from './other.js';");
        plan.push_file(barrel);
        // Two real owners of `Y` → ambiguous → no owner.
        plan.push_file(file_with_export("modules/island/a.ts", "Y"));
        plan.push_file(file_with_export("modules/island/b.ts", "Y"));
        let index = BindingOwnerIndex::from_plan(&plan, HUB);
        assert_eq!(index.owner(&BindingName::new("X")), None);
        assert_eq!(index.owner(&BindingName::new("Y")), None);
    }

    #[test]
    fn index_excludes_hub_and_eager_chunks() {
        let mut plan = EmitPlan::default();
        let mut hub = file_with_export(HUB, "fromHub");
        hub.push_source("export { fromHub };");
        plan.push_file(hub);
        let mut chunk = file_with_export("modules/island/chunk.ts", "fromChunk");
        chunk.eager_ordered_chunk = true;
        plan.push_file(chunk);
        let index = BindingOwnerIndex::from_plan(&plan, HUB);
        assert_eq!(index.owner(&BindingName::new("fromHub")), None);
        assert_eq!(index.owner(&BindingName::new("fromChunk")), None);
    }

    #[test]
    fn reroutes_consumer_hub_import_to_owner() {
        let mut plan = EmitPlan::default();
        plan.push_file(PlannedFile::new(HUB));
        plan.push_file(file_with_export("modules/island/auth/oauth.ts", "Wa"));
        plan.push_file(consumer_importing_from_hub(
            "modules/account.ts",
            "import { Wa } from './entrypoint.js';",
        ));
        reroute_entrypoint_island_barrel(&mut plan);
        assert_eq!(
            body_of(&plan, "modules/account.ts"),
            "import { Wa } from './island/auth/oauth.js';"
        );
    }

    #[test]
    fn splits_mixed_line_keeping_unowned_on_hub() {
        let mut plan = EmitPlan::default();
        plan.push_file(PlannedFile::new(HUB));
        plan.push_file(file_with_export("modules/island/auth/oauth.ts", "Wa"));
        // `Unowned` has no owner (only the hub would provide it) → stays on hub.
        plan.push_file(consumer_importing_from_hub(
            "modules/account.ts",
            "import { Unowned, Wa } from './entrypoint.js';",
        ));
        reroute_entrypoint_island_barrel(&mut plan);
        let body = body_of(&plan, "modules/account.ts");
        assert!(
            body.contains("import { Unowned } from './entrypoint.js';"),
            "unowned stays on hub: {body}"
        );
        assert!(
            body.contains("import { Wa } from './island/auth/oauth.js';"),
            "owned reroutes: {body}"
        );
    }

    #[test]
    fn chunk_owned_binding_stays_on_hub() {
        let mut plan = EmitPlan::default();
        plan.push_file(PlannedFile::new(HUB));
        let mut chunk = file_with_export("modules/island/chunk.ts", "Eager");
        chunk.eager_ordered_chunk = true;
        plan.push_file(chunk);
        plan.push_file(consumer_importing_from_hub(
            "modules/account.ts",
            "import { Eager } from './entrypoint.js';",
        ));
        reroute_entrypoint_island_barrel(&mut plan);
        assert_eq!(
            body_of(&plan, "modules/account.ts"),
            "import { Eager } from './entrypoint.js';"
        );
    }

    #[test]
    fn preserves_consumer_alias_when_rerouting() {
        let mut plan = EmitPlan::default();
        plan.push_file(PlannedFile::new(HUB));
        plan.push_file(file_with_export("modules/island/auth/oauth.ts", "Wa"));
        plan.push_file(consumer_importing_from_hub(
            "modules/account.ts",
            "import { Wa as oauthError } from './entrypoint.js';",
        ));
        reroute_entrypoint_island_barrel(&mut plan);
        assert_eq!(
            body_of(&plan, "modules/account.ts"),
            "import { Wa as oauthError } from './island/auth/oauth.js';"
        );
    }

    #[test]
    fn is_idempotent() {
        let mut plan = EmitPlan::default();
        plan.push_file(PlannedFile::new(HUB));
        plan.push_file(file_with_export("modules/island/auth/oauth.ts", "Wa"));
        plan.push_file(consumer_importing_from_hub(
            "modules/account.ts",
            "import { Wa } from './entrypoint.js';",
        ));
        reroute_entrypoint_island_barrel(&mut plan);
        let once = body_of(&plan, "modules/account.ts").to_string();
        reroute_entrypoint_island_barrel(&mut plan);
        assert_eq!(body_of(&plan, "modules/account.ts"), once);
    }
}
