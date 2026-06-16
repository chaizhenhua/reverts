//! Persist matcher-synthesized `ModuleInput` rows into the `modules`
//! SQLite table. The matcher emits synthetic modules for bundle wrappers
//! that don't have a row yet; they must land in the table before
//! `package_attributions.module_id` FKs can resolve.

use reverts_input::ModuleInput;
use reverts_ir::ModuleKind;
use rusqlite::{Connection, params};

use crate::errors::MatchPackagesError;

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
