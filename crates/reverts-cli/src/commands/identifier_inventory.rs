//! `identifier-inventory` subcommand: count AST identifier sites in emitted code.
//!
//! This command fills the gap between module-scope semantic naming progress and
//! the rest of the generated JavaScript/TypeScript AST. It is deliberately
//! read-only and AST-backed.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use reverts_js::{IdentifierInventoryStats, ParseGoal, collect_identifier_inventory};
use serde_json::Value;

use crate::args::IdentifierInventoryArgs;
use crate::errors::CliRunError;

pub(crate) fn run(args: IdentifierInventoryArgs) -> Result<(), CliRunError> {
    let json = identifier_inventory_json(&args)?;
    if let Some(path) = &args.json {
        fs::write(path, json).map_err(|source| CliRunError::WriteOutput {
            path: path.clone(),
            source,
        })?;
    } else {
        println!("{json}");
    }
    Ok(())
}

pub fn identifier_inventory_json(args: &IdentifierInventoryArgs) -> Result<String, CliRunError> {
    let report = identifier_inventory_report(args)?;
    serde_json::to_string_pretty(&report)
        .map(|json| format!("{json}\n"))
        .map_err(|source| CliRunError::IdentifierInventory(source.to_string()))
}

pub fn identifier_inventory_report(args: &IdentifierInventoryArgs) -> Result<Value, CliRunError> {
    let mut files = code_files(args.output_root.as_path())?;
    files.sort();
    let semantic_name_index = load_explicit_semantic_name_index(args.output_root.as_path())?;
    let semantic_binding_decisions =
        load_semantic_binding_decision_index(args.output_root.as_path())?;
    let first_party_files = load_first_party_file_set(args.first_party_files.as_deref())?;

    let mut totals = IdentifierInventoryStats::default();
    let mut semantic_total = 0_usize;
    let mut semantic_named = 0_usize;
    let mut semantic_preserved = 0_usize;
    let mut semantic_pending = 0_usize;
    let mut semantic_excluded = 0_usize;
    let mut semantic_pending_files = Vec::new();
    let mut scanned_files = Vec::new();
    let mut parse_errors = Vec::new();

    for path in files {
        let source = fs::read_to_string(&path).map_err(|source| {
            CliRunError::IdentifierInventory(format!("failed to read {}: {source}", path.display()))
        })?;
        match collect_identifier_inventory(&source, Some(path.as_path()), ParseGoal::TypeScript) {
            Ok(stats) => {
                let relative_path = relative_display(args.output_root.as_path(), path.as_path());
                let semantic = semantic_binding_file_coverage(
                    relative_path.as_str(),
                    &stats,
                    semantic_name_index.get(relative_path.as_str()),
                    semantic_binding_decisions.get(relative_path.as_str()),
                    first_party_files.as_ref(),
                );
                semantic_total += semantic.total;
                semantic_named += semantic.named;
                semantic_preserved += semantic.preserved;
                semantic_pending += semantic.pending;
                semantic_excluded += semantic.excluded;
                if semantic.pending > 0 {
                    semantic_pending_files.push(serde_json::json!({
                        "path": relative_path,
                        "total": semantic.total,
                        "named": semantic.named,
                        "preserved": semantic.preserved,
                        "pending": semantic.pending,
                        "reason": semantic.reason,
                        "pending_bindings": semantic.pending_bindings,
                    }));
                }
                totals.binding_identifiers += stats.binding_identifiers;
                totals.identifier_references += stats.identifier_references;
                totals.static_member_properties += stats.static_member_properties;
                totals.object_property_keys += stats.object_property_keys;
                totals.import_specifiers += stats.import_specifiers;
                totals.export_specifiers += stats.export_specifiers;
                totals.semantic_named_bindings += stats.semantic_named_bindings;
                totals.semantic_pending_bindings += stats.semantic_pending_bindings;
                scanned_files.push(serde_json::json!({
                    "path": relative_path,
                    "identifiers": stats.total(),
                    "binding_identifiers": stats.binding_identifiers,
                    "identifier_references": stats.identifier_references,
                    "static_member_properties": stats.static_member_properties,
                    "object_property_keys": stats.object_property_keys,
                    "import_specifiers": stats.import_specifiers,
                    "export_specifiers": stats.export_specifiers,
                    "semantic_bindings": {
                        "total": semantic.total,
                        "named": semantic.named,
                        "preserved": semantic.preserved,
                        "excluded": semantic.excluded,
                        "complete_count": semantic.complete_count(),
                        "pending": semantic.pending,
                        "percent": percent(semantic.complete_count(), semantic.total),
                        "complete": semantic.pending == 0,
                        "reason": semantic.reason,
                        "pending_bindings": semantic.pending_bindings,
                    },
                }));
            }
            Err(error) => parse_errors.push(serde_json::json!({
                "path": relative_display(args.output_root.as_path(), path.as_path()),
                "message": error.to_string(),
            })),
        }
    }

    Ok(serde_json::json!({
        "schema": "reverts.identifier_inventory.v1",
        "artifacts": {
            "output_root": args.output_root,
        },
        "files": {
            "scanned": scanned_files.len(),
            "parse_errors": parse_errors.len(),
        },
        "identifiers": {
            "total": totals.total(),
            "binding_identifiers": totals.binding_identifiers,
            "identifier_references": totals.identifier_references,
            "static_member_properties": totals.static_member_properties,
            "object_property_keys": totals.object_property_keys,
            "import_specifiers": totals.import_specifiers,
            "export_specifiers": totals.export_specifiers,
            "complete": parse_errors.is_empty(),
            "pending": 0,
        },
        "semantic_bindings": {
            "total": semantic_total,
            "named": semantic_named,
            "preserved": semantic_preserved,
            "excluded": semantic_excluded,
            "complete_count": semantic_named + semantic_preserved + semantic_excluded,
            "pending": semantic_pending,
            "files_with_pending": semantic_pending_files.len(),
            "pending_files": semantic_pending_files,
            "percent": percent(semantic_named + semantic_preserved + semantic_excluded, semantic_total),
            "complete": parse_errors.is_empty() && semantic_pending == 0,
        },
        "by_file": scanned_files,
        "parse_errors": parse_errors,
        "complete": parse_errors.is_empty(),
    }))
}

struct SemanticBindingCoverage {
    total: usize,
    named: usize,
    preserved: usize,
    excluded: usize,
    pending: usize,
    reason: &'static str,
    pending_bindings: Vec<Value>,
}

impl SemanticBindingCoverage {
    const fn complete_count(&self) -> usize {
        self.named + self.preserved + self.excluded
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SemanticBindingDecisionStatus {
    Named,
    Preserved,
    Excluded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SemanticBindingDecision {
    status: SemanticBindingDecisionStatus,
    count: Option<usize>,
}

fn semantic_binding_file_coverage(
    relative_path: &str,
    stats: &IdentifierInventoryStats,
    explicit_names: Option<&BTreeMap<String, usize>>,
    binding_decisions: Option<&BTreeMap<String, Vec<SemanticBindingDecision>>>,
    first_party_files: Option<&std::collections::BTreeSet<String>>,
) -> SemanticBindingCoverage {
    if !is_semantic_binding_target_path(relative_path) {
        return SemanticBindingCoverage {
            total: 0,
            named: 0,
            preserved: 0,
            excluded: stats.binding_identifiers,
            pending: 0,
            reason: "generated_scaffold_not_decompiled_target",
            pending_bindings: Vec::new(),
        };
    }
    // When a first-party allow-list is supplied, bindings in non-first-party
    // files (bundled third-party / classified-out modules) are not naming work:
    // count them as excluded so they never gate coverage completion.
    if let Some(first_party) = first_party_files
        && !first_party.contains(relative_path)
    {
        return SemanticBindingCoverage {
            total: 0,
            named: 0,
            preserved: 0,
            excluded: stats.binding_identifiers,
            pending: 0,
            reason: "not_first_party_module",
            pending_bindings: Vec::new(),
        };
    }
    let mut pending_binding_names = stats.semantic_pending_binding_names.clone();
    let mut consumed_by_name = BTreeMap::<String, usize>::new();
    let mut named = 0_usize;
    let mut preserved = 0_usize;
    let mut excluded = 0_usize;
    if let Some(explicit_names) = explicit_names {
        for (name, accepted_count) in explicit_names {
            let Some(pending_count) = pending_binding_names.get_mut(name) else {
                continue;
            };
            let consumed = (*pending_count).min(*accepted_count);
            *pending_count -= consumed;
            *consumed_by_name.entry(name.clone()).or_default() += consumed;
            named += consumed;
        }
    }
    if let Some(binding_decisions) = binding_decisions {
        for (name, decisions) in binding_decisions {
            let Some(pending_count) = pending_binding_names.get_mut(name) else {
                continue;
            };
            for decision in decisions {
                if *pending_count == 0 {
                    break;
                }
                let requested = decision.count.unwrap_or(*pending_count);
                let consumed = requested.min(*pending_count);
                *pending_count -= consumed;
                *consumed_by_name.entry(name.clone()).or_default() += consumed;
                match decision.status {
                    SemanticBindingDecisionStatus::Named => named += consumed,
                    SemanticBindingDecisionStatus::Preserved => preserved += consumed,
                    SemanticBindingDecisionStatus::Excluded => excluded += consumed,
                }
            }
        }
    }
    pending_binding_names.retain(|_, count| *count > 0);
    let pending = stats
        .binding_identifiers
        .saturating_sub(named + preserved + excluded);
    SemanticBindingCoverage {
        total: stats.binding_identifiers,
        named,
        preserved,
        excluded,
        pending,
        reason: if binding_decisions.is_some() {
            "explicit_semantic_name_and_binding_decisions"
        } else {
            "explicit_semantic_name_bindings_only"
        },
        pending_bindings: pending_binding_entries(stats, &mut consumed_by_name),
    }
}

fn pending_binding_entries(
    stats: &IdentifierInventoryStats,
    consumed_by_name: &mut BTreeMap<String, usize>,
) -> Vec<Value> {
    let mut entries = Vec::new();
    for binding in &stats.semantic_pending_binding_entries {
        let consumed = consumed_by_name
            .entry(binding.original_name.clone())
            .or_default();
        if *consumed > 0 {
            *consumed -= 1;
            continue;
        }
        entries.push(serde_json::json!({
            "original_name": binding.original_name,
            "binding_index": binding.binding_index,
            "count": 1,
        }));
    }
    entries
}

fn load_semantic_binding_decision_index(
    output_root: &Path,
) -> Result<BTreeMap<String, BTreeMap<String, Vec<SemanticBindingDecision>>>, CliRunError> {
    let path = output_root
        .join("semantic-binding-index.json")
        .exists()
        .then(|| output_root.join("semantic-binding-index.json"))
        .or_else(|| {
            output_root
                .parent()
                .map(|parent| parent.join("semantic-binding-index.json"))
                .filter(|path| path.exists())
        });
    let Some(path) = path else {
        return Ok(BTreeMap::new());
    };
    let text = fs::read_to_string(&path).map_err(|source| {
        CliRunError::IdentifierInventory(format!("failed to read {}: {source}", path.display()))
    })?;
    let value: Value = serde_json::from_str(&text).map_err(|source| {
        CliRunError::IdentifierInventory(format!("failed to parse {}: {source}", path.display()))
    })?;
    let files = value
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            CliRunError::IdentifierInventory(format!(
                "{} must contain a files array",
                path.display()
            ))
        })?;
    let mut index = BTreeMap::<String, BTreeMap<String, Vec<SemanticBindingDecision>>>::new();
    for file in files {
        let file_path = file.get("path").and_then(Value::as_str).ok_or_else(|| {
            CliRunError::IdentifierInventory(format!(
                "{} file entries must contain path",
                path.display()
            ))
        })?;
        let bindings = file
            .get("bindings")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                CliRunError::IdentifierInventory(format!(
                    "{} file entries must contain bindings",
                    path.display()
                ))
            })?;
        for binding in bindings {
            let original_name = binding
                .get("original_name")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    CliRunError::IdentifierInventory(format!(
                        "{} binding entries must contain original_name",
                        path.display()
                    ))
                })?;
            let status = binding
                .get("status")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    CliRunError::IdentifierInventory(format!(
                        "{} binding entries must contain status",
                        path.display()
                    ))
                })
                .and_then(|status| parse_semantic_binding_decision_status(status, &path))?;
            let count = binding
                .get("count")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok());
            index
                .entry(file_path.to_string())
                .or_default()
                .entry(original_name.to_string())
                .or_default()
                .push(SemanticBindingDecision { status, count });
        }
    }
    Ok(index)
}

fn parse_semantic_binding_decision_status(
    status: &str,
    path: &Path,
) -> Result<SemanticBindingDecisionStatus, CliRunError> {
    match status {
        "named" | "accept" | "accepted" => Ok(SemanticBindingDecisionStatus::Named),
        "preserved" | "preserve" => Ok(SemanticBindingDecisionStatus::Preserved),
        "excluded" | "exclude" => Ok(SemanticBindingDecisionStatus::Excluded),
        _ => Err(CliRunError::IdentifierInventory(format!(
            "{} has unsupported semantic binding status {status:?}",
            path.display()
        ))),
    }
}

fn is_semantic_binding_target_path(relative_path: &str) -> bool {
    !relative_path.is_empty()
}

/// Load the optional first-party allow-list (newline-separated output-relative
/// paths). `None` means "no filter — every file counts" (current behavior).
fn load_first_party_file_set(
    path: Option<&Path>,
) -> Result<Option<std::collections::BTreeSet<String>>, CliRunError> {
    let Some(path) = path else {
        return Ok(None);
    };
    let text = fs::read_to_string(path).map_err(|source| {
        CliRunError::IdentifierInventory(format!(
            "failed to read first-party file list {}: {source}",
            path.display()
        ))
    })?;
    let set = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    Ok(Some(set))
}

fn load_explicit_semantic_name_index(
    output_root: &Path,
) -> Result<BTreeMap<String, BTreeMap<String, usize>>, CliRunError> {
    let mut index = BTreeMap::<String, BTreeMap<String, usize>>::new();
    load_explicit_semantic_name_index_file(output_root, "symbol-index.json", &mut index)?;
    load_explicit_semantic_name_index_file(output_root, "binding-name-index.json", &mut index)?;
    Ok(index)
}

fn load_explicit_semantic_name_index_file(
    output_root: &Path,
    file_name: &str,
    index: &mut BTreeMap<String, BTreeMap<String, usize>>,
) -> Result<(), CliRunError> {
    let path = output_root.join(file_name);
    if !path.exists() {
        return Ok(());
    }
    let text = fs::read_to_string(&path).map_err(|source| {
        CliRunError::IdentifierInventory(format!("failed to read {}: {source}", path.display()))
    })?;
    let value: Value = serde_json::from_str(&text).map_err(|source| {
        CliRunError::IdentifierInventory(format!("failed to parse {}: {source}", path.display()))
    })?;
    let Some(rows) = value.as_array() else {
        return Ok(());
    };
    for row in rows {
        if !row
            .get("semantic_named")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let Some(file_path) = row.get("file_path").and_then(Value::as_str) else {
            continue;
        };
        let Some(emitted_name) = row.get("emitted_name").and_then(Value::as_str) else {
            continue;
        };
        let accepted_count = if file_name == "binding-name-index.json"
            && row.get("binding_index").and_then(Value::as_u64).is_some()
        {
            1
        } else if file_name == "binding-name-index.json" {
            usize::MAX
        } else {
            1
        };
        let name_index = index
            .entry(file_path.to_string())
            .or_default()
            .entry(emitted_name.to_string())
            .or_default();
        *name_index = name_index.saturating_add(accepted_count);
    }
    Ok(())
}

fn percent(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        100.0
    } else {
        (numerator as f64 * 100.0) / denominator as f64
    }
}

fn code_files(root: &Path) -> Result<Vec<PathBuf>, CliRunError> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    let mut stack = vec![PathBuf::from(root)];
    while let Some(next) = stack.pop() {
        let metadata = fs::symlink_metadata(&next).map_err(|source| {
            CliRunError::IdentifierInventory(format!("failed to stat {}: {source}", next.display()))
        })?;
        if metadata.is_file() {
            if is_code_path(&next) {
                files.push(next);
            }
        } else if metadata.is_dir() && !should_skip_dir(root, &next) {
            let entries = fs::read_dir(&next).map_err(|source| {
                CliRunError::IdentifierInventory(format!(
                    "failed to read {}: {source}",
                    next.display()
                ))
            })?;
            for entry in entries {
                let entry = entry.map_err(|source| {
                    CliRunError::IdentifierInventory(format!(
                        "failed to read entry under {}: {source}",
                        next.display()
                    ))
                })?;
                stack.push(entry.path());
            }
        }
    }
    Ok(files)
}

fn should_skip_dir(root: &Path, path: &Path) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative.components().any(|component| {
        let component = component.as_os_str().to_string_lossy();
        component.as_ref() == ".git"
    })
}

fn is_code_path(path: &Path) -> bool {
    path.extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| {
            matches!(
                extension,
                "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts"
            )
        })
}

fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn scans_all_emitted_code_including_dependency_and_dist_trees() {
        let temp = tempdir().expect("temp dir");
        let root = temp.path();
        fs::create_dir(root.join("modules")).expect("mkdir modules");
        fs::write(
            root.join("modules/index.ts"),
            "const answer = 42; console.log(answer);",
        )
        .expect("write index");
        fs::create_dir(root.join("node_modules")).expect("mkdir node_modules");
        fs::write(root.join("node_modules/pkg.js"), "const a = 1;").expect("write package");
        fs::create_dir(root.join("dist")).expect("mkdir dist");
        fs::write(root.join("dist/bundle.js"), "const b = 1;").expect("write dist");

        let report = identifier_inventory_report(&IdentifierInventoryArgs {
            output_root: root.to_path_buf(),
            json: None,
            first_party_files: None,
        })
        .expect("inventory should run");

        assert_eq!(
            report
                .get("files")
                .and_then(|files| files.get("scanned"))
                .and_then(Value::as_u64),
            Some(3)
        );
        assert_eq!(
            report
                .get("parse_errors")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(0)
        );
        assert!(
            report
                .get("identifiers")
                .and_then(|identifiers| identifiers.get("total"))
                .and_then(Value::as_u64)
                .is_some_and(|count| count > 0)
        );
        assert!(
            report
                .get("semantic_bindings")
                .and_then(|bindings| bindings.get("total"))
                .and_then(Value::as_u64)
                .is_some_and(|count| count > 0)
        );
        assert_eq!(
            report
                .get("semantic_bindings")
                .and_then(|bindings| bindings.get("files_with_pending"))
                .and_then(Value::as_u64),
            Some(3)
        );
    }

    #[test]
    fn reports_pending_semantic_binding_files() {
        let temp = tempdir().expect("temp dir");
        let root = temp.path();
        fs::create_dir(root.join("modules")).expect("mkdir modules");
        fs::write(
            root.join("modules/minified.ts"),
            "const a = 1; function b(c) { return a + c; }",
        )
        .expect("write module");

        let report = identifier_inventory_report(&IdentifierInventoryArgs {
            output_root: root.to_path_buf(),
            json: None,
            first_party_files: None,
        })
        .expect("inventory should run");

        assert_eq!(
            report
                .get("semantic_bindings")
                .and_then(|bindings| bindings.get("files_with_pending"))
                .and_then(Value::as_u64),
            Some(1)
        );
        let pending_files = report
            .get("semantic_bindings")
            .and_then(|bindings| bindings.get("pending_files"))
            .and_then(Value::as_array)
            .expect("pending files should be listed");
        assert_eq!(pending_files[0]["path"], "modules/minified.ts");
        assert_eq!(pending_files[0]["pending"], 3);
    }

    #[test]
    fn template_literal_files_still_count_minified_bindings_as_pending() {
        let temp = tempdir().expect("temp dir");
        let root = temp.path();
        fs::create_dir(root.join("modules")).expect("mkdir modules");
        fs::write(
            root.join("modules/template.ts"),
            "const a = `literal`; function b(c) { return `${a}${c}`; }",
        )
        .expect("write module");

        let report = identifier_inventory_report(&IdentifierInventoryArgs {
            output_root: root.to_path_buf(),
            json: None,
            first_party_files: None,
        })
        .expect("inventory should run");

        assert_eq!(
            report
                .get("semantic_bindings")
                .and_then(|bindings| bindings.get("pending"))
                .and_then(Value::as_u64),
            Some(3)
        );
        let pending_files = report
            .get("semantic_bindings")
            .and_then(|bindings| bindings.get("pending_files"))
            .and_then(Value::as_array)
            .expect("pending files should be listed");
        assert_eq!(pending_files[0]["path"], "modules/template.ts");
        assert_eq!(
            pending_files[0]["reason"],
            "explicit_semantic_name_bindings_only"
        );
    }

    #[test]
    fn counts_only_explicit_semantic_name_index_rows_as_named() {
        let temp = tempdir().expect("temp dir");
        let root = temp.path();
        fs::create_dir(root.join("modules")).expect("mkdir modules");
        fs::write(
            root.join("modules/minified.ts"),
            "const createClient = 1; function b(c) { return createClient + c; }",
        )
        .expect("write module");
        fs::write(
            root.join("symbol-index.json"),
            serde_json::json!([
                {
                    "module_id": 1,
                    "original_name": "a",
                    "emitted_name": "createClient",
                    "semantic_named": true,
                    "file_path": "modules/minified.ts"
                },
                {
                    "module_id": 1,
                    "original_name": "b",
                    "emitted_name": "b",
                    "semantic_named": false,
                    "file_path": "modules/minified.ts"
                }
            ])
            .to_string(),
        )
        .expect("write symbol index");

        let report = identifier_inventory_report(&IdentifierInventoryArgs {
            output_root: root.to_path_buf(),
            json: None,
            first_party_files: None,
        })
        .expect("inventory should run");

        assert_eq!(
            report["semantic_bindings"]["named"].as_u64(),
            Some(1),
            "only semantic_named=true rows count as named"
        );
        assert_eq!(report["semantic_bindings"]["pending"].as_u64(), Some(2));
        let bindings = report["semantic_bindings"]["pending_files"][0]["pending_bindings"]
            .as_array()
            .expect("pending binding names");
        assert!(
            !bindings
                .iter()
                .any(|binding| binding["original_name"] == "createClient" && binding["count"] == 1)
        );
        assert!(
            bindings
                .iter()
                .any(|binding| binding["original_name"] == "b" && binding["count"] == 1)
        );
        assert!(
            bindings
                .iter()
                .any(|binding| binding["original_name"] == "c" && binding["count"] == 1)
        );
    }

    #[test]
    fn reports_pending_binding_names_per_file() {
        let temp = tempdir().expect("temp dir");
        let root = temp.path();
        fs::create_dir(root.join("modules")).expect("mkdir modules");
        fs::write(
            root.join("modules/minified.ts"),
            "const a = 1; function b(a) { return a; }",
        )
        .expect("write module");

        let report = identifier_inventory_report(&IdentifierInventoryArgs {
            output_root: root.to_path_buf(),
            json: None,
            first_party_files: None,
        })
        .expect("inventory should run");

        let pending_files = report
            .get("semantic_bindings")
            .and_then(|bindings| bindings.get("pending_files"))
            .and_then(Value::as_array)
            .expect("pending files should be listed");
        let bindings = pending_files[0]["pending_bindings"]
            .as_array()
            .expect("pending binding names should be listed");

        assert!(bindings.iter().any(|binding| {
            binding["original_name"] == "a"
                && binding["binding_index"] == 1
                && binding["count"] == 1
        }));
        assert!(bindings.iter().any(|binding| {
            binding["original_name"] == "a"
                && binding["binding_index"] == 2
                && binding["count"] == 1
        }));
        assert!(bindings.iter().any(|binding| {
            binding["original_name"] == "b"
                && binding["binding_index"] == 1
                && binding["count"] == 1
        }));
    }

    #[test]
    fn semantic_binding_index_marks_generated_bindings_complete() {
        let temp = tempdir().expect("temp dir");
        let root = temp.path();
        fs::create_dir(root.join("modules")).expect("mkdir modules");
        fs::write(
            root.join("modules/minified.ts"),
            "const a = 1; function b(c) { return a + c; }",
        )
        .expect("write module");
        fs::write(
            root.join("semantic-binding-index.json"),
            serde_json::json!({
                "schema": "reverts.semantic_binding_index.v1",
                "files": [
                    {
                        "path": "modules/minified.ts",
                        "bindings": [
                            {"original_name": "a", "status": "named", "semantic_name": "answer"},
                            {"original_name": "b", "status": "preserved"},
                            {"original_name": "c", "status": "excluded"}
                        ]
                    }
                ]
            })
            .to_string(),
        )
        .expect("write semantic binding index");

        let report = identifier_inventory_report(&IdentifierInventoryArgs {
            output_root: root.to_path_buf(),
            json: None,
            first_party_files: None,
        })
        .expect("inventory should run");

        assert_eq!(report["semantic_bindings"]["total"].as_u64(), Some(3));
        assert_eq!(report["semantic_bindings"]["named"].as_u64(), Some(1));
        assert_eq!(report["semantic_bindings"]["preserved"].as_u64(), Some(1));
        assert_eq!(report["semantic_bindings"]["excluded"].as_u64(), Some(1));
        assert_eq!(
            report["semantic_bindings"]["complete_count"].as_u64(),
            Some(3)
        );
        assert_eq!(report["semantic_bindings"]["pending"].as_u64(), Some(0));
        assert_eq!(
            report["semantic_bindings"]["complete"].as_bool(),
            Some(true)
        );
    }

    #[test]
    fn first_party_files_filter_excludes_non_first_party_bindings_from_coverage() {
        let temp = tempdir().expect("temp dir");
        let root = temp.path();
        fs::create_dir(root.join("modules")).expect("mkdir modules");
        // app.ts is first-party (3 bindings); vendor.ts is bundled third-party.
        fs::write(
            root.join("modules/app.ts"),
            "const a = 1; const b = 2; const c = 3;",
        )
        .expect("write app");
        fs::write(root.join("modules/vendor.ts"), "const x = 1; const y = 2;")
            .expect("write vendor");
        let list = root.join("first-party.txt");
        fs::write(&list, "modules/app.ts\n").expect("write list");

        let report = identifier_inventory_report(&IdentifierInventoryArgs {
            output_root: root.to_path_buf(),
            json: None,
            first_party_files: Some(list.clone()),
        })
        .expect("inventory should run");

        // Only app.ts (3) is pending; vendor.ts (2) is excluded, not pending.
        assert_eq!(report["semantic_bindings"]["pending"].as_u64(), Some(3));
        assert_eq!(report["semantic_bindings"]["excluded"].as_u64(), Some(2));

        // Without the filter, all 5 are pending.
        let unfiltered = identifier_inventory_report(&IdentifierInventoryArgs {
            output_root: root.to_path_buf(),
            json: None,
            first_party_files: None,
        })
        .expect("inventory should run");
        assert_eq!(unfiltered["semantic_bindings"]["pending"].as_u64(), Some(5));
    }
}
