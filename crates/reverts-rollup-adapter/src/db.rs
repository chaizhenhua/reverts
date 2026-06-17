use reverts_analyze::rollup::model::{AttributionRow, HintRow, ModuleRow, Snapshot};
use rusqlite::Connection;

pub fn load_snapshot(conn: &Connection) -> Result<Snapshot, rusqlite::Error> {
    let mut stmt =
        conn.prepare("SELECT id, module_category, package_name, package_version FROM modules")?;
    let modules = stmt
        .query_map([], |row| {
            Ok(ModuleRow {
                id: row.get(0)?,
                category: row.get(1)?,
                package_name: row.get(2)?,
                package_version: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);

    let mut stmt = conn.prepare(
        "SELECT module_id, package_name, package_version, export_specifier, emission_mode, status, evidence_json, rejection_reason FROM package_attributions",
    )?;
    let attributions = stmt
        .query_map([], |row| {
            Ok(AttributionRow {
                module_id: row.get(0)?,
                package_name: row.get(1)?,
                package_version: row.get(2)?,
                export_specifier: row.get(3)?,
                emission_mode: row.get(4)?,
                status: row.get(5)?,
                evidence_json: row.get(6)?,
                rejection_reason: row.get(7)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);

    let mut stmt = conn.prepare(
        "SELECT package_name, package_version, export_specifier, public_members_json FROM package_externalization_hints",
    )?;
    let hints = stmt
        .query_map([], |row| {
            let members_json: String = row.get(3)?;
            let public_members: Vec<String> =
                serde_json::from_str(&members_json).unwrap_or_default();
            Ok(HintRow {
                package_name: row.get(0)?,
                package_version: row.get(1)?,
                export_specifier: row.get(2)?,
                public_members,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);

    Ok(Snapshot {
        modules,
        attributions,
        hints,
    })
}
