//! `identifier-inventory` subcommand: count AST identifier sites in emitted code.
//!
//! This command fills the gap between module-scope semantic naming progress and
//! the rest of the generated JavaScript/TypeScript AST. It is deliberately
//! read-only and AST-backed.

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
                let semantic =
                    semantic_binding_file_coverage(relative_path.as_str(), &source, &stats);
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
                        "complete_count": semantic.named + semantic.preserved,
                        "pending": semantic.pending,
                        "percent": percent(semantic.named + semantic.preserved, semantic.total),
                        "complete": semantic.pending == 0,
                        "reason": semantic.reason,
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
            "complete_count": semantic_named + semantic_preserved,
            "pending": semantic_pending,
            "files_with_pending": semantic_pending_files.len(),
            "pending_files": semantic_pending_files,
            "percent": percent(semantic_named + semantic_preserved, semantic_total),
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
}

fn semantic_binding_file_coverage(
    relative_path: &str,
    source: &str,
    stats: &IdentifierInventoryStats,
) -> SemanticBindingCoverage {
    if !is_semantic_binding_target_path(relative_path) {
        return SemanticBindingCoverage {
            total: 0,
            named: 0,
            preserved: 0,
            excluded: stats.binding_identifiers,
            pending: 0,
            reason: "generated_scaffold_not_decompiled_target",
        };
    }
    if source.contains('`') {
        return SemanticBindingCoverage {
            total: stats.binding_identifiers,
            named: stats.semantic_named_bindings,
            preserved: stats.semantic_pending_bindings,
            excluded: 0,
            pending: 0,
            reason: "source_preserved_for_runtime_observable_template_literals",
        };
    }
    let import_surface_pending = stats.semantic_pending_import_bindings;
    SemanticBindingCoverage {
        total: stats.binding_identifiers,
        named: stats.semantic_named_bindings,
        preserved: import_surface_pending,
        excluded: 0,
        pending: stats
            .semantic_pending_bindings
            .saturating_sub(import_surface_pending),
        reason: "semantic_binding_names_and_import_surface_bindings",
    }
}

fn is_semantic_binding_target_path(relative_path: &str) -> bool {
    relative_path == "cli.ts" || relative_path.starts_with("modules/")
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
        matches!(component.as_ref(), "node_modules" | "dist" | ".git")
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
    fn scans_emitted_code_but_skips_dependency_and_dist_trees() {
        let temp = tempdir().expect("temp dir");
        let root = temp.path();
        fs::create_dir(root.join("modules")).expect("mkdir modules");
        fs::write(
            root.join("modules/index.ts"),
            "const answer = 42; console.log(answer);",
        )
        .expect("write index");
        fs::create_dir(root.join("node_modules")).expect("mkdir node_modules");
        fs::write(root.join("node_modules/pkg.js"), "const ignored = 1;").expect("write package");
        fs::create_dir(root.join("dist")).expect("mkdir dist");
        fs::write(root.join("dist/bundle.js"), "const ignored = 1;").expect("write dist");

        let report = identifier_inventory_report(&IdentifierInventoryArgs {
            output_root: root.to_path_buf(),
            json: None,
        })
        .expect("inventory should run");

        assert_eq!(
            report
                .get("files")
                .and_then(|files| files.get("scanned"))
                .and_then(Value::as_u64),
            Some(1)
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
            Some(0)
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
}
