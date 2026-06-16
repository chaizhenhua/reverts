//! Persist accepted package-surface decisions into `package_surfaces`.
//! One row per (project, export_specifier) pair the matcher accepted.

use reverts_input::{InputRows, PackageSurfaceInput};
use reverts_package_matcher::VersionedPackageMatchReport;
use rusqlite::{Connection, params};

use crate::errors::MatchPackagesError;

pub(crate) fn persist_package_surfaces(
    connection: &mut Connection,
    rows: &InputRows,
    report: &VersionedPackageMatchReport,
) -> Result<usize, MatchPackagesError> {
    if report.surfaces.is_empty() {
        return Ok(0);
    }

    ensure_package_surfaces_table(connection)?;
    let transaction = connection
        .transaction()
        .map_err(MatchPackagesError::WritePackageSurface)?;
    let mut written = 0;
    for surface in &report.surfaces {
        persist_package_surface(&transaction, rows.project.id, surface)?;
        written += 1;
    }
    transaction
        .commit()
        .map_err(MatchPackagesError::WritePackageSurface)?;
    Ok(written)
}

fn ensure_package_surfaces_table(connection: &Connection) -> Result<(), MatchPackagesError> {
    connection
        .execute_batch(
            r"
            CREATE TABLE IF NOT EXISTS package_surfaces (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_id INTEGER NOT NULL,
                package_name TEXT NOT NULL,
                package_version TEXT NOT NULL,
                export_specifier TEXT NOT NULL,
                status TEXT NOT NULL,
                evidence_json TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                UNIQUE (project_id, export_specifier)
            );
            ",
        )
        .map_err(MatchPackagesError::WritePackageSurface)
}

fn persist_package_surface(
    connection: &Connection,
    project_id: u32,
    surface: &PackageSurfaceInput,
) -> Result<(), MatchPackagesError> {
    let package_version =
        surface
            .package_version
            .as_deref()
            .ok_or(MatchPackagesError::InvalidPackageSurface {
                export_specifier: surface.export_specifier.clone(),
                message: "accepted package surface has no package version".to_string(),
            })?;
    let evidence = surface.evidence.clone().unwrap_or_else(|| {
        serde_json::json!({
            "matcher": "source_package_import_surface",
            "package_name": surface.package_name.as_str(),
            "package_version": package_version,
            "export_specifier": surface.export_specifier.as_str(),
        })
        .to_string()
    });
    connection
        .execute(
            r"
            INSERT INTO package_surfaces
                (project_id, package_name, package_version, export_specifier,
                 status, evidence_json, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, 'accepted', ?5, datetime('now'), datetime('now'))
            ON CONFLICT(project_id, export_specifier) DO UPDATE SET
                package_name = excluded.package_name,
                package_version = excluded.package_version,
                status = excluded.status,
                evidence_json = excluded.evidence_json,
                updated_at = datetime('now')
            ",
            params![
                i64::from(project_id),
                surface.package_name.as_str(),
                package_version,
                surface.export_specifier.as_str(),
                evidence,
            ],
        )
        .map_err(MatchPackagesError::WritePackageSurface)?;
    Ok(())
}
