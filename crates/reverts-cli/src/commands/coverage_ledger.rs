//! `coverage-ledger` subcommand: write the unified decompile coverage ledger.
//!
//! The ledger is a dashboard over existing authoritative artifacts. It does not
//! replace SQLite, the unpack evidence manifest, or symbol-index sidecars; it
//! normalizes their completion state into one status model so users can see why
//! `module_symbol: 599/599` is only one part of whole-target coverage.

use std::fs;

use serde_json::Value;

use crate::args::{CoverageLedgerArgs, FullInventoryArgs};
use crate::commands::full_inventory::full_inventory_report;
use crate::errors::CliRunError;

pub(crate) fn run(args: CoverageLedgerArgs) -> Result<(), CliRunError> {
    let json = coverage_ledger_json(&args)?;
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

pub fn coverage_ledger_json(args: &CoverageLedgerArgs) -> Result<String, CliRunError> {
    let report = coverage_ledger_report(args)?;
    serde_json::to_string_pretty(&report)
        .map(|json| format!("{json}\n"))
        .map_err(|source| CliRunError::CoverageLedger(source.to_string()))
}

pub fn coverage_ledger_report(args: &CoverageLedgerArgs) -> Result<Value, CliRunError> {
    let inventory = load_or_build_inventory(args)?;
    Ok(ledger_from_inventory(&inventory))
}

fn load_or_build_inventory(args: &CoverageLedgerArgs) -> Result<Value, CliRunError> {
    if let Some(path) = &args.full_inventory {
        let text = fs::read_to_string(path).map_err(|source| {
            CliRunError::CoverageLedger(format!(
                "failed to read full inventory {}: {source}",
                path.display()
            ))
        })?;
        return serde_json::from_str::<Value>(&text).map_err(|source| {
            CliRunError::CoverageLedger(format!(
                "failed to parse full inventory {}: {source}",
                path.display()
            ))
        });
    }
    full_inventory_report(&FullInventoryArgs {
        input: args.input.clone(),
        project_id: args.project_id,
        manifest: args.manifest.clone(),
        source_root: args.source_root.clone(),
        output_root: args.output_root.clone(),
        naming_progress: args.naming_progress.clone(),
        json: None,
    })
}

fn ledger_from_inventory(inventory: &Value) -> Value {
    let files = inventory.get("files").unwrap_or(&Value::Null);
    let modules = inventory.get("modules").unwrap_or(&Value::Null);
    let packages = inventory.get("packages").unwrap_or(&Value::Null);
    let symbols = inventory.get("symbols").unwrap_or(&Value::Null);

    let rows = [
        coverage_row(
            "input_file",
            number(files, "unpack_manifest_dir_files"),
            number(files, "unpack_manifest_dir_files"),
            0,
        ),
        coverage_row(
            "reverts_source_file",
            number(files, "reverts_source_files"),
            number(files, "reverts_source_files"),
            0,
        ),
        coverage_row(
            "reverts_asset",
            number(files, "reverts_assets"),
            number(files, "reverts_assets"),
            0,
        ),
        coverage_row(
            "module",
            number(modules, "total"),
            number(modules, "total").saturating_sub(number(modules, "unclassified")),
            number(modules, "unclassified"),
        ),
        coverage_row(
            "package_module",
            number(packages, "package_modules"),
            number(packages, "matched"),
            number(packages, "unmatched"),
        ),
        coverage_row(
            "module_symbol",
            number(symbols, "semantic_required"),
            number(symbols, "semantic_named"),
            number(symbols, "semantic_pending"),
        ),
        coverage_row(
            "generated_output_file",
            number(files, "output_files"),
            number(files, "output_files"),
            0,
        ),
    ];
    let by_kind = rows
        .iter()
        .map(|row| {
            (
                row.kind.to_string(),
                serde_json::json!({
                    "total": row.total,
                    "complete": row.complete,
                    "pending": row.pending,
                    "blocked": row.blocked,
                    "status": if row.pending == 0 && row.blocked == 0 { "complete" } else { "pending" },
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let total_items = rows.iter().map(|row| row.total).sum::<usize>();
    let complete_items = rows.iter().map(|row| row.complete).sum::<usize>();
    let pending_items = rows.iter().map(|row| row.pending).sum::<usize>();
    let blocked_items = rows.iter().map(|row| row.blocked).sum::<usize>();
    let item_groups = rows
        .iter()
        .map(|row| {
            serde_json::json!({
                "id": format!("group:{}", row.kind),
                "kind": row.kind,
                "count": row.total,
                "complete": row.complete,
                "pending": row.pending,
                "blocked": row.blocked,
                "status": if row.pending == 0 && row.blocked == 0 { "complete" } else { "pending" },
                "required_action": if row.pending == 0 { Value::Null } else { Value::String(required_action(row.kind).to_string()) },
                "reason": group_reason(row.kind),
            })
        })
        .collect::<Vec<_>>();
    let pending = item_groups
        .iter()
        .filter(|item| {
            item.get("pending")
                .and_then(Value::as_u64)
                .is_some_and(|count| count > 0)
        })
        .cloned()
        .collect::<Vec<_>>();

    serde_json::json!({
        "schema": "reverts.coverage_ledger.v1",
        "project_id": inventory.get("project_id").cloned().unwrap_or(Value::Null),
        "status": if pending_items == 0 && blocked_items == 0 { "complete" } else { "pending" },
        "summary": {
            "total_items": total_items,
            "complete_items": complete_items,
            "pending_items": pending_items,
            "blocked_items": blocked_items,
        },
        "by_kind": by_kind,
        "items": item_groups,
        "pending_items": pending,
        "source": {
            "full_inventory_schema": inventory.get("schema").cloned().unwrap_or(Value::Null),
            "full_inventory_complete": inventory.get("complete").cloned().unwrap_or(Value::Bool(false)),
        },
    })
}

#[derive(Clone, Copy)]
struct CoverageRow {
    kind: &'static str,
    total: usize,
    complete: usize,
    pending: usize,
    blocked: usize,
}

fn coverage_row(kind: &'static str, total: usize, complete: usize, pending: usize) -> CoverageRow {
    CoverageRow {
        kind,
        total,
        complete,
        pending,
        blocked: 0,
    }
}

fn number(value: &Value, key: &str) -> usize {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0)
}

fn required_action(kind: &str) -> &'static str {
    match kind {
        "module" => "classify",
        "package_module" => "match_package",
        "module_symbol" => "name",
        _ => "explain",
    }
}

fn group_reason(kind: &str) -> &'static str {
    match kind {
        "input_file" => "covered by unpack evidence or explained by file scope",
        "reverts_source_file" => "imported as Reverts source facts",
        "reverts_asset" => "preserved or materialized as non-AST assets",
        "module" => "classified as application/package/runtime/third-party",
        "package_module" => "matched or pending package attribution",
        "module_symbol" => "covered by emitted first-party module-level semantic naming",
        "generated_output_file" => {
            "emitted by generate-project-v2 before validation dependencies/build outputs"
        }
        _ => "covered by decompile ledger",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn ledger_summarizes_inventory_dimensions() {
        let inventory = json!({
            "schema": "reverts.full_inventory.v1",
            "project_id": 1,
            "complete": true,
            "files": {
                "unpack_manifest_dir_files": 10,
                "reverts_source_files": 3,
                "reverts_assets": 2,
                "output_files": 4
            },
            "modules": {"total": 5, "unclassified": 1},
            "packages": {"package_modules": 2, "matched": 2, "unmatched": 0},
            "symbols": {"semantic_required": 7, "semantic_named": 6, "semantic_pending": 1}
        });

        let ledger = super::ledger_from_inventory(&inventory);

        assert_eq!(ledger["status"], "pending");
        assert_eq!(ledger["by_kind"]["module"]["pending"], 1);
        assert_eq!(ledger["by_kind"]["module_symbol"]["pending"], 1);
        assert_eq!(ledger["summary"]["pending_items"], 2);
    }
}
