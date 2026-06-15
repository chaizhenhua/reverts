use rusqlite::Connection;

#[derive(Debug, Clone)]
pub struct ModuleRow {
    pub id: i64,
    pub category: String,
    pub package_name: Option<String>,
    pub package_version: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AttributionRow {
    pub module_id: i64,
    pub package_name: String,
    pub package_version: Option<String>,
    pub export_specifier: Option<String>,
    pub emission_mode: String,
    pub status: String,
    pub evidence_json: Option<String>,
    pub rejection_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HintRow {
    pub package_name: String,
    pub package_version: String,
    pub export_specifier: String,
    pub public_members: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub modules: Vec<ModuleRow>,
    pub attributions: Vec<AttributionRow>,
    pub hints: Vec<HintRow>,
}

pub fn load_snapshot(conn: &Connection) -> Result<Snapshot, rusqlite::Error> {
    // Each of these tables may be absent in callers that build small ad-hoc
    // databases (matcher test fixtures, in-memory probes). Treat any missing
    // table as an empty input; the oracle naturally produces no rollups in
    // that case which keeps the rollup pass a safe no-op.
    let modules = if table_exists(conn, "modules")? {
        let mut stmt =
            conn.prepare("SELECT id, module_category, package_name, package_version FROM modules")?;
        stmt.query_map([], |row| {
            Ok(ModuleRow {
                id: row.get(0)?,
                category: row.get(1)?,
                package_name: row.get(2)?,
                package_version: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };

    let attributions = if table_exists(conn, "package_attributions")? {
        let mut stmt = conn.prepare(
            "SELECT module_id, package_name, package_version, export_specifier, emission_mode, status, evidence_json, rejection_reason FROM package_attributions",
        )?;
        stmt.query_map([], |row| {
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
        .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };

    let hints = if table_exists(conn, "package_externalization_hints")? {
        let mut stmt = conn.prepare(
            "SELECT package_name, package_version, export_specifier, public_members_json FROM package_externalization_hints",
        )?;
        stmt.query_map([], |row| {
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
        .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };

    Ok(Snapshot {
        modules,
        attributions,
        hints,
    })
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool, rusqlite::Error> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}
