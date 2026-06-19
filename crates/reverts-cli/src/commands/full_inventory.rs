//! `full-inventory` subcommand: write the decompile session's coverage report.
//!
//! This command is deliberately a report over existing facts. SQLite remains
//! the source of truth for imported Reverts facts, the unpack evidence manifest
//! remains the source of truth for unpacked inputs, and `symbol-index.json`
//! remains the source of truth for emitted symbols.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, params};
use serde_json::Value;

use crate::args::{FullInventoryArgs, NamingProgressArgs, NamingProgressTier};
use crate::commands::naming_progress::{naming_progress_from_sqlite, naming_progress_json};
use crate::errors::CliRunError;

pub(crate) fn run(args: FullInventoryArgs) -> Result<(), CliRunError> {
    let json = full_inventory_json(&args)?;
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

pub fn full_inventory_json(args: &FullInventoryArgs) -> Result<String, CliRunError> {
    let report = full_inventory_report(args)?;
    serde_json::to_string_pretty(&report)
        .map(|json| format!("{json}\n"))
        .map_err(|source| CliRunError::FullInventory(source.to_string()))
}

pub fn full_inventory_report(args: &FullInventoryArgs) -> Result<Value, CliRunError> {
    let connection = Connection::open_with_flags(&args.input, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|source| {
            CliRunError::FullInventory(format!("failed to open {}: {source}", args.input.display()))
        })?;

    let manifest = args
        .manifest
        .as_ref()
        .map(|path| manifest_counts(path.as_path()))
        .transpose()?
        .unwrap_or_default();
    let source_root_files = optional_file_count(args.source_root.as_deref())?;
    let output_counts = output_file_counts(args.output_root.as_deref())?;
    let symbol_index_entries = args
        .output_root
        .as_ref()
        .map(|path| symbol_index_count(path.as_path()))
        .transpose()?
        .unwrap_or(0);

    let reverts_source_files = scalar(
        &connection,
        "SELECT COUNT(*) FROM project_files WHERE project_id = ?1",
        args.project_id,
    )?;
    let reverts_assets = scalar(
        &connection,
        "SELECT COUNT(*) FROM project_assets WHERE project_id = ?1",
        args.project_id,
    )?;
    let modules_total = scalar(
        &connection,
        "SELECT COUNT(*) FROM modules WHERE ?1 > 0",
        args.project_id,
    )?;
    let unclassified_modules = scalar(
        &connection,
        r#"
        SELECT COUNT(*)
        FROM modules m
        WHERE ?1 > 0
          AND (
            m.module_category IS NULL
            OR trim(m.module_category) = ''
            OR m.module_category NOT IN (
              'application',
              'package',
              'third-party-library',
              'runtime-glue'
            )
          )
        "#,
        args.project_id,
    )?;
    let application_modules = module_category_count(&connection, args.project_id, "application")?;
    let package_category_modules = module_category_count(&connection, args.project_id, "package")?;
    let third_party_modules =
        module_category_count(&connection, args.project_id, "third-party-library")?;
    let runtime_glue_modules = module_category_count(&connection, args.project_id, "runtime-glue")?;

    let package_modules = scalar(
        &connection,
        "SELECT COUNT(*) FROM modules m WHERE ?1 > 0 AND (m.package_name IS NOT NULL OR m.module_category = 'package')",
        args.project_id,
    )?;
    let unmatched_package_modules = scalar(
        &connection,
        r#"
        SELECT COUNT(*)
        FROM modules m
        LEFT JOIN package_attributions pa ON pa.module_id = m.id
        WHERE ?1 > 0
          AND (m.package_name IS NOT NULL OR m.module_category = 'package')
          AND pa.module_id IS NULL
        "#,
        args.project_id,
    )?;
    let package_attributions = scalar(
        &connection,
        "SELECT COUNT(*) FROM package_attributions WHERE ?1 > 0",
        args.project_id,
    )?;
    let package_surfaces = scalar(
        &connection,
        "SELECT COUNT(*) FROM package_surfaces WHERE project_id = ?1",
        args.project_id,
    )?;

    let progress_json = if let Some(path) = &args.naming_progress {
        let text = fs::read_to_string(path).map_err(|source| {
            CliRunError::FullInventory(format!(
                "failed to read naming progress {}: {source}",
                path.display()
            ))
        })?;
        serde_json::from_str::<Value>(&text).map_err(|source| {
            CliRunError::FullInventory(format!(
                "failed to parse naming progress {}: {source}",
                path.display()
            ))
        })?
    } else {
        let progress_args = NamingProgressArgs {
            input: args.input.clone(),
            project_id: args.project_id,
            target_level: NamingProgressTier::Full,
            json: true,
        };
        let progress = naming_progress_from_sqlite(&progress_args)
            .map_err(|source| CliRunError::FullInventory(source.to_string()))?;
        serde_json::from_str::<Value>(&naming_progress_json(&progress, NamingProgressTier::Full))
            .map_err(|source| CliRunError::FullInventory(source.to_string()))?
    };
    let semantic_total = json_usize(&progress_json, &["total"]);
    let semantic_named = json_usize(&progress_json, &["named"]);
    let semantic_pending = json_usize(&progress_json, &["pending"]);
    let semantic_complete = progress_json
        .get("complete")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let complete = unclassified_modules == 0 && unmatched_package_modules == 0 && semantic_complete;
    Ok(serde_json::json!({
        "schema": "reverts.full_inventory.v1",
        "project_id": args.project_id,
        "artifacts": {
            "database": args.input,
            "manifest": args.manifest,
            "source_root": args.source_root,
            "output_root": args.output_root,
            "symbol_index": args.output_root.as_ref().map(|root| root.join("symbol-index.json")),
        },
        "files": {
            "unpack_manifest_dir_files": manifest.manifest_dir_files,
            "dmg_extracted_files": manifest.dmg_extracted_files,
            "input_app_files": manifest.input_app_files,
            "unpacked_app_files": manifest.output_app_files,
            "manifest_sources": manifest.sources,
            "manifest_assets": manifest.assets,
            "manifest_native_assets": manifest.native_assets,
            "manifest_packages": manifest.packages,
            "source_root_files": source_root_files,
            "reverts_source_files": reverts_source_files,
            "reverts_assets": reverts_assets,
            "output_files": output_counts.total_files,
            "output_root_node_modules_files": output_counts.root_node_modules_files,
            "output_generated_files": output_counts.generated_files,
            "output_dist_files": output_counts.dist_files,
        },
        "modules": {
            "total": modules_total,
            "application": application_modules,
            "package": package_category_modules,
            "third_party_library": third_party_modules,
            "runtime_glue": runtime_glue_modules,
            "unclassified": unclassified_modules,
        },
        "packages": {
            "package_modules": package_modules,
            "matched": package_modules.saturating_sub(unmatched_package_modules),
            "unmatched": unmatched_package_modules,
            "attributions": package_attributions,
            "surfaces": package_surfaces,
        },
        "symbols": {
            "symbol_index_entries": symbol_index_entries,
            "semantic_required": semantic_total,
            "semantic_named": semantic_named,
            "semantic_pending": semantic_pending,
            "semantic_complete": semantic_complete,
        },
        "excluded_or_explained": [
            {
                "kind": "native_binary",
                "count": manifest.native_assets,
                "reason": "preserved as native assets; not JS/TS AST semantic-name targets"
            },
            {
                "kind": "static_asset",
                "count": manifest.assets,
                "reason": "preserved as emitted assets; not JS/TS AST semantic-name targets"
            },
            {
                "kind": "third_party_package",
                "count": package_attributions,
                "reason": "matched by package attribution and externalized or explained by package emission mode"
            }
        ],
        "complete": complete,
    }))
}

#[derive(Default)]
struct ManifestCounts {
    manifest_dir_files: usize,
    dmg_extracted_files: usize,
    input_app_files: usize,
    output_app_files: usize,
    sources: usize,
    assets: usize,
    native_assets: usize,
    packages: usize,
}

fn manifest_counts(path: &Path) -> Result<ManifestCounts, CliRunError> {
    let text = fs::read_to_string(path).map_err(|source| {
        CliRunError::FullInventory(format!(
            "failed to read manifest {}: {source}",
            path.display()
        ))
    })?;
    let value: Value = serde_json::from_str(&text).map_err(|source| {
        CliRunError::FullInventory(format!(
            "failed to parse manifest {}: {source}",
            path.display()
        ))
    })?;
    let manifest_dir_files = path
        .parent()
        .map(recursive_file_count)
        .transpose()?
        .unwrap_or(0);
    let dmg_extracted_files = path
        .parent()
        .map(|parent| parent.join("dmg-extracted"))
        .as_deref()
        .map(recursive_file_count)
        .transpose()?
        .unwrap_or(0);
    let input_app_files = json_path(&value, "input_app")
        .map(|path| recursive_file_count(path.as_path()))
        .transpose()?
        .unwrap_or(0);
    let output_app_files = json_path(&value, "output_app")
        .map(|path| recursive_file_count(path.as_path()))
        .transpose()?
        .unwrap_or(0);
    Ok(ManifestCounts {
        manifest_dir_files,
        dmg_extracted_files,
        input_app_files,
        output_app_files,
        sources: json_array_len(&value, "sources"),
        assets: json_array_len(&value, "assets"),
        native_assets: json_array_len(&value, "native_assets"),
        packages: json_array_len(&value, "packages"),
    })
}

fn json_array_len(value: &Value, key: &str) -> usize {
    value.get(key).and_then(Value::as_array).map_or(0, Vec::len)
}

fn json_path(value: &Value, key: &str) -> Option<PathBuf> {
    value.get(key).and_then(Value::as_str).map(PathBuf::from)
}

fn scalar(connection: &Connection, sql: &str, project_id: u32) -> Result<usize, CliRunError> {
    connection
        .query_row(sql, params![project_id], |row| row.get::<_, i64>(0))
        .map(|value| usize::try_from(value).unwrap_or(0))
        .map_err(|source| CliRunError::FullInventory(source.to_string()))
}

fn module_category_count(
    connection: &Connection,
    project_id: u32,
    category: &str,
) -> Result<usize, CliRunError> {
    connection
        .query_row(
            "SELECT COUNT(*) FROM modules m WHERE ?1 > 0 AND m.module_category = ?2",
            params![project_id, category],
            |row| row.get::<_, i64>(0),
        )
        .map(|value| usize::try_from(value).unwrap_or(0))
        .map_err(|source| CliRunError::FullInventory(source.to_string()))
}

fn optional_file_count(path: Option<&Path>) -> Result<usize, CliRunError> {
    match path {
        Some(path) => recursive_file_count(path),
        None => Ok(0),
    }
}

#[derive(Default)]
struct OutputFileCounts {
    total_files: usize,
    root_node_modules_files: usize,
    generated_files: usize,
    dist_files: usize,
}

fn output_file_counts(path: Option<&Path>) -> Result<OutputFileCounts, CliRunError> {
    let Some(root) = path else {
        return Ok(OutputFileCounts::default());
    };
    if !root.exists() {
        return Ok(OutputFileCounts::default());
    }
    let mut counts = OutputFileCounts::default();
    let mut stack = vec![PathBuf::from(root)];
    while let Some(next) = stack.pop() {
        let metadata = fs::symlink_metadata(&next).map_err(|source| {
            CliRunError::FullInventory(format!("failed to stat {}: {source}", next.display()))
        })?;
        if metadata.is_file() {
            counts.total_files += 1;
            let relative = next.strip_prefix(root).unwrap_or(next.as_path());
            if first_component_is(relative, "node_modules") {
                counts.root_node_modules_files += 1;
            } else if first_component_is(relative, "dist") {
                counts.dist_files += 1;
            } else {
                counts.generated_files += 1;
            }
        } else if metadata.is_dir() {
            let entries = fs::read_dir(&next).map_err(|source| {
                CliRunError::FullInventory(format!("failed to read {}: {source}", next.display()))
            })?;
            for entry in entries {
                let entry = entry.map_err(|source| {
                    CliRunError::FullInventory(format!(
                        "failed to read entry under {}: {source}",
                        next.display()
                    ))
                })?;
                stack.push(entry.path());
            }
        }
    }
    Ok(counts)
}

fn first_component_is(path: &Path, component: &str) -> bool {
    path.components()
        .next()
        .is_some_and(|part| part.as_os_str().to_string_lossy() == component)
}

fn recursive_file_count(path: &Path) -> Result<usize, CliRunError> {
    if !path.exists() {
        return Ok(0);
    }
    let mut count = 0_usize;
    let mut stack = vec![PathBuf::from(path)];
    while let Some(next) = stack.pop() {
        let metadata = fs::symlink_metadata(&next).map_err(|source| {
            CliRunError::FullInventory(format!("failed to stat {}: {source}", next.display()))
        })?;
        if metadata.is_file() {
            count += 1;
        } else if metadata.is_dir() {
            let entries = fs::read_dir(&next).map_err(|source| {
                CliRunError::FullInventory(format!("failed to read {}: {source}", next.display()))
            })?;
            for entry in entries {
                let entry = entry.map_err(|source| {
                    CliRunError::FullInventory(format!(
                        "failed to read entry under {}: {source}",
                        next.display()
                    ))
                })?;
                stack.push(entry.path());
            }
        }
    }
    Ok(count)
}

fn symbol_index_count(output_root: &Path) -> Result<usize, CliRunError> {
    let path = output_root.join("symbol-index.json");
    if !path.exists() {
        return Ok(0);
    }
    let text = fs::read_to_string(&path).map_err(|source| {
        CliRunError::FullInventory(format!("failed to read {}: {source}", path.display()))
    })?;
    let value: Value = serde_json::from_str(&text).map_err(|source| {
        CliRunError::FullInventory(format!("failed to parse {}: {source}", path.display()))
    })?;
    Ok(value.as_array().map_or(0, Vec::len))
}

fn json_usize(value: &Value, path: &[&str]) -> usize {
    let mut cursor = value;
    for key in path {
        if let Some(next) = cursor.get(*key) {
            cursor = next;
        } else {
            return 0;
        }
    }
    cursor
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn counts_manifest_arrays() {
        let value = json!({
            "sources": [{}, {}],
            "assets": [{}],
            "native_assets": [{}, {}, {}],
            "packages": []
        });

        assert_eq!(super::json_array_len(&value, "sources"), 2);
        assert_eq!(super::json_array_len(&value, "assets"), 1);
        assert_eq!(super::json_array_len(&value, "native_assets"), 3);
        assert_eq!(super::json_array_len(&value, "packages"), 0);
    }

    #[test]
    fn first_component_detection_matches_only_top_level_segments() {
        assert!(super::first_component_is(
            std::path::Path::new("node_modules/pkg/index.js"),
            "node_modules"
        ));
        assert!(super::first_component_is(
            std::path::Path::new("dist/modules/a.js"),
            "dist"
        ));
        assert!(!super::first_component_is(
            std::path::Path::new("assets/node_modules/pkg/readme.txt"),
            "node_modules"
        ));
    }
}
