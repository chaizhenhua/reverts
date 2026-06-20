//! Persist matcher-synthesized `ModuleInput` rows into the `modules`
//! SQLite table. The matcher emits synthetic modules for bundle wrappers
//! that don't have a row yet; they must land in the table before
//! `package_attributions.module_id` FKs can resolve.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use reverts_input::{InputRows, ModuleInput, SourceFileInput};
use reverts_ir::ModuleKind;
use rusqlite::{Connection, params};

use crate::errors::MatchPackagesError;
use crate::sqlite_table_has_column;

const SYNTHETIC_SOURCE_PREFIX: &str = "__reverts_synthetic__/";

pub(crate) fn persist_prepared_synthetic_inputs(
    connection: &mut Connection,
    project_id: u32,
    rows: &InputRows,
    synthetic_modules: &[ModuleInput],
) -> Result<usize, MatchPackagesError> {
    if synthetic_modules.is_empty() {
        return Ok(0);
    }
    persist_synthetic_source_files(connection, project_id, rows, synthetic_modules)?;
    persist_synthetic_modules(connection, synthetic_modules)
}

fn persist_synthetic_source_files(
    connection: &mut Connection,
    project_id: u32,
    rows: &InputRows,
    synthetic_modules: &[ModuleInput],
) -> Result<(), MatchPackagesError> {
    let source_file_ids = synthetic_modules
        .iter()
        .filter_map(|module| module.source_file_id)
        .collect::<BTreeSet<_>>();
    if source_file_ids.is_empty() {
        return Ok(());
    }
    let db_dir = database_parent_dir(connection)?;
    let source_files = rows
        .source_files
        .iter()
        .filter(|source_file| source_file_ids.contains(&source_file.id))
        .collect::<Vec<_>>();
    let materialized = source_files
        .iter()
        .map(|source_file| materialize_source_file(&db_dir, project_id, source_file))
        .collect::<Result<Vec<_>, _>>()?;

    let has_file_size = sqlite_table_has_column(connection, "source_files", "file_size")
        .map_err(MatchPackagesError::WriteAttribution)?;
    let transaction = connection
        .transaction()
        .map_err(MatchPackagesError::WriteAttribution)?;
    for (source_file, path, size) in materialized {
        if has_file_size {
            transaction
                .execute(
                    r"
                    INSERT INTO source_files (id, file_path, file_size)
                    VALUES (?1, ?2, ?3)
                    ON CONFLICT(id) DO UPDATE SET
                        file_path = excluded.file_path,
                        file_size = excluded.file_size
                    ",
                    params![i64::from(source_file.id), path, size],
                )
                .map_err(MatchPackagesError::WriteAttribution)?;
        } else {
            transaction
                .execute(
                    r"
                    INSERT INTO source_files (id, file_path)
                    VALUES (?1, ?2)
                    ON CONFLICT(id) DO UPDATE SET file_path = excluded.file_path
                    ",
                    params![i64::from(source_file.id), path],
                )
                .map_err(MatchPackagesError::WriteAttribution)?;
        }
        transaction
            .execute(
                r"
                INSERT INTO project_files (project_id, file_id)
                SELECT ?1, ?2
                WHERE NOT EXISTS (
                    SELECT 1 FROM project_files WHERE project_id = ?1 AND file_id = ?2
                )
                ",
                params![i64::from(project_id), i64::from(source_file.id)],
            )
            .map_err(MatchPackagesError::WriteAttribution)?;
    }
    transaction
        .commit()
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(())
}

fn database_parent_dir(connection: &Connection) -> Result<PathBuf, MatchPackagesError> {
    let db_path = connection
        .query_row("PRAGMA database_list", [], |row| row.get::<_, String>(2))
        .map_err(MatchPackagesError::WriteAttribution)?;
    if db_path.is_empty() {
        return Ok(std::env::temp_dir().join("reverts-synthetic-sources"));
    }
    Ok(PathBuf::from(db_path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf())
}

fn materialize_source_file<'a>(
    db_dir: &Path,
    project_id: u32,
    source_file: &'a SourceFileInput,
) -> Result<(&'a SourceFileInput, String, i64), MatchPackagesError> {
    if let Some(source) = source_file.source.as_deref()
        && source_file.path.starts_with(SYNTHETIC_SOURCE_PREFIX)
    {
        let dir = db_dir
            .join(".reverts-synthetic-sources")
            .join(format!("project-{project_id}"));
        std::fs::create_dir_all(dir.as_path()).map_err(|source| {
            MatchPackagesError::WriteSyntheticSource {
                path: dir.clone(),
                source,
            }
        })?;
        let path = dir.join(format!("source-{}.js", source_file.id));
        std::fs::write(path.as_path(), source).map_err(|source| {
            MatchPackagesError::WriteSyntheticSource {
                path: path.clone(),
                source,
            }
        })?;
        let size =
            i64::try_from(source.len()).map_err(|_| MatchPackagesError::InvalidAttribution {
                module_id: reverts_ir::ModuleId(source_file.id),
                message: "synthetic source is too large for SQLite file_size".to_string(),
            })?;
        return Ok((source_file, path.to_string_lossy().into_owned(), size));
    }

    let size = source_file
        .source
        .as_ref()
        .map(|source| i64::try_from(source.len()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    Ok((source_file, source_file.path.clone(), size))
}

pub(crate) fn persist_synthetic_modules(
    connection: &mut Connection,
    synthetic_modules: &[ModuleInput],
) -> Result<usize, MatchPackagesError> {
    if synthetic_modules.is_empty() {
        return Ok(0);
    }
    let transaction = connection
        .transaction()
        .map_err(MatchPackagesError::WriteAttribution)?;
    let mut written = 0usize;
    for module in synthetic_modules {
        let Some(span) = module.source_span else {
            continue;
        };
        let kind_str = match module.kind {
            ModuleKind::Application => "application",
            ModuleKind::Package => "package",
            ModuleKind::Builtin => "builtin",
        };
        let n = transaction
            .execute(
                r"
                INSERT OR IGNORE INTO modules
                    (id, file_id, original_name, semantic_name, module_category,
                     package_name, package_version, byte_start, byte_end,
                     created_at, updated_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                        datetime('now'), datetime('now'))
                ",
                params![
                    module.id.0,
                    module.source_file_id,
                    module.original_name,
                    module.semantic_path,
                    kind_str,
                    module.package_name,
                    module.package_version,
                    span.byte_start,
                    span.byte_end,
                ],
            )
            .map_err(MatchPackagesError::WriteAttribution)?;
        written += n;
    }
    transaction
        .commit()
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(written)
}
