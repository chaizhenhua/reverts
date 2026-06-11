use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::{
    AssetRow, DatabaseRows, InputBundle, InputBundleError, InputRows, ModuleDependencyRow,
    ModuleDependencyRowTarget, ModuleRow, PackageAttributionRow, PackageAttributionStatus,
    PackageEmissionMode, PackageSurfaceRow, ProjectRow, SourceFileRow, StoredModuleKind, SymbolRow,
};

pub fn load_project_bundle_from_sqlite(
    path: impl AsRef<Path>,
    project_id: u32,
) -> Result<InputBundle, SqliteInputError> {
    let path = path.as_ref();
    let connection =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|source| {
            SqliteInputError::OpenDatabase {
                path: path.to_path_buf(),
                source,
            }
        })?;
    load_project_bundle_from_connection(&connection, project_id)
}

pub fn load_project_rows_from_sqlite(
    path: impl AsRef<Path>,
    project_id: u32,
) -> Result<InputRows, SqliteInputError> {
    let path = path.as_ref();
    let connection =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|source| {
            SqliteInputError::OpenDatabase {
                path: path.to_path_buf(),
                source,
            }
        })?;
    load_project_rows_from_connection(&connection, project_id)
}

pub fn load_project_bundle_from_connection(
    connection: &Connection,
    project_id: u32,
) -> Result<InputBundle, SqliteInputError> {
    let rows = load_project_rows_from_connection(connection, project_id)?;
    InputBundle::from_rows(rows).map_err(SqliteInputError::InputBundle)
}

pub fn load_project_rows_from_connection(
    connection: &Connection,
    project_id: u32,
) -> Result<InputRows, SqliteInputError> {
    let mut rows = DatabaseRows::new(load_project(connection, project_id)?);
    rows.source_files = load_source_files(connection, project_id)?;
    rows.modules = load_modules(connection, project_id)?;
    rows.symbols = load_module_symbols(connection, project_id)?;
    rows.dependencies = load_module_dependencies(connection, project_id)?;
    rows.package_attributions = load_package_attributions(connection, project_id)?;
    rows.package_surfaces = load_package_surfaces(connection, project_id)?;
    rows.assets = load_project_assets(connection, project_id)?;
    InputRows::from_database_rows(rows).map_err(SqliteInputError::InputBundle)
}

fn load_project(connection: &Connection, project_id: u32) -> Result<ProjectRow, SqliteInputError> {
    connection
        .query_row(
            "SELECT id, name FROM projects WHERE id = ?1",
            params![i64::from(project_id)],
            |row| {
                Ok(ProjectRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                })
            },
        )
        .optional()?
        .ok_or(SqliteInputError::ProjectNotFound { project_id })
}

fn load_source_files(
    connection: &Connection,
    project_id: u32,
) -> Result<Vec<SourceFileRow>, SqliteInputError> {
    let mut statement = connection.prepare(
        r"
        SELECT sf.id, pf.project_id, sf.file_path
        FROM source_files sf
        JOIN project_files pf ON pf.file_id = sf.id
        WHERE pf.project_id = ?1
        ORDER BY sf.id
        ",
    )?;
    let rows = statement.query_map(params![i64::from(project_id)], |row| {
        let path = row.get::<_, String>(2)?;
        let source = fs::read_to_string(path.as_str()).map_err(|source| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(ReadSourceError {
                path: PathBuf::from(path.clone()),
                source,
            }))
        })?;
        Ok(SourceFileRow {
            id: row.get(0)?,
            project_id: row.get(1)?,
            path,
            source: Some(source),
        })
    })?;

    collect_sqlite_rows(rows)
}

fn load_modules(
    connection: &Connection,
    project_id: u32,
) -> Result<Vec<ModuleRow>, SqliteInputError> {
    let mut statement = connection.prepare(
        r"
        SELECT
            m.id,
            m.file_id,
            m.original_name,
            m.semantic_name,
            m.module_category,
            m.package_name,
            m.package_version,
            m.byte_start,
            m.byte_end
        FROM modules m
        JOIN project_files pf ON pf.file_id = m.file_id
        WHERE pf.project_id = ?1
        ORDER BY m.id
        ",
    )?;
    let rows = statement.query_map(params![i64::from(project_id)], |row| {
        let id = row.get::<_, i64>(0)?;
        let original_name = row.get::<_, String>(2)?;
        let semantic_name = row.get::<_, Option<String>>(3)?;
        let semantic_path = module_semantic_path(id, semantic_name.as_deref(), &original_name);
        let category = row.get::<_, Option<String>>(4)?;

        Ok(ModuleRow {
            id,
            source_file_id: row.get(1)?,
            original_name,
            semantic_path: Some(semantic_path),
            kind: module_kind_from_category(category.as_deref()),
            package_name: row.get(5)?,
            package_version: row.get(6)?,
            byte_start: row.get(7)?,
            byte_end: row.get(8)?,
        })
    })?;

    collect_sqlite_rows(rows)
}

fn load_module_symbols(
    connection: &Connection,
    project_id: u32,
) -> Result<Vec<SymbolRow>, SqliteInputError> {
    let mut statement = connection.prepare(
        r"
        SELECT DISTINCT
            s.module_id,
            s.original_name AS symbol_name,
            NULLIF(TRIM(s.semantic_name), '') AS semantic_name,
            NULLIF(TRIM(s.export_name), '') AS export_name
        FROM symbols s
        JOIN modules m ON m.id = s.module_id
        JOIN project_files pf ON pf.file_id = m.file_id
        WHERE pf.project_id = ?1
          AND s.module_id IS NOT NULL
          AND s.scope_level = 'module'
          AND TRIM(s.original_name) != ''
        ORDER BY s.module_id, symbol_name, semantic_name, export_name
        ",
    )?;
    let rows = statement.query_map(params![i64::from(project_id)], |row| {
        Ok(SymbolRow {
            module_id: row.get(0)?,
            name: row.get(1)?,
            semantic_name: row.get(2)?,
            export_name: row.get(3)?,
        })
    })?;

    collect_sqlite_rows(rows)
}

fn load_module_dependencies(
    connection: &Connection,
    project_id: u32,
) -> Result<Vec<ModuleDependencyRow>, SqliteInputError> {
    let mut statement = connection.prepare(
        r"
        SELECT md.module_id, md.dependency_id
        FROM module_dependencies md
        JOIN modules source_module ON source_module.id = md.module_id
        JOIN project_files source_project ON source_project.file_id = source_module.file_id
        JOIN modules target_module ON target_module.id = md.dependency_id
        JOIN project_files target_project ON target_project.file_id = target_module.file_id
        WHERE source_project.project_id = ?1
          AND target_project.project_id = ?1
        ORDER BY md.module_id, md.dependency_id
        ",
    )?;
    let rows = statement.query_map(params![i64::from(project_id)], |row| {
        Ok(ModuleDependencyRow {
            from_module_id: row.get(0)?,
            target: ModuleDependencyRowTarget::Module {
                module_id: row.get(1)?,
            },
        })
    })?;

    collect_sqlite_rows(rows)
}

// Function-level cascade attributions (`function_span` + `confidence`)
// live in the separate `package_function_attributions` table written by
// `reverts-cli::persist_cascade_attributions`. This loader handles only
// the legacy module-level rows; a cascade-aware loader would be a
// separate function so the schemas stay decoupled.
fn load_package_attributions(
    connection: &Connection,
    project_id: u32,
) -> Result<Vec<PackageAttributionRow>, SqliteInputError> {
    if !table_exists(connection, "package_attributions")? {
        return Ok(Vec::new());
    }

    let mut statement = connection.prepare(
        r"
        SELECT
            pa.module_id,
            pa.package_name,
            pa.package_version,
            pa.package_subpath,
            pa.export_specifier,
            pa.emission_mode,
            pa.status,
            pa.rejection_reason
        FROM package_attributions pa
        JOIN modules m ON m.id = pa.module_id
        JOIN project_files pf ON pf.file_id = m.file_id
        WHERE pf.project_id = ?1
        ORDER BY pa.module_id
        ",
    )?;
    let rows = statement.query_map(params![i64::from(project_id)], |row| {
        let emission_mode = emission_mode_from_database(row.get::<_, String>(5)?.as_str())?;
        let status = attribution_status_from_database(row.get::<_, String>(6)?.as_str())?;
        Ok(PackageAttributionRow {
            module_id: row.get(0)?,
            package_name: row.get(1)?,
            package_version: row.get(2)?,
            subpath: row.get(3)?,
            export_specifier: row.get(4)?,
            emission_mode,
            status,
            rejection_reason: row.get(7)?,
        })
    })?;

    collect_sqlite_rows(rows)
}

fn load_package_surfaces(
    connection: &Connection,
    project_id: u32,
) -> Result<Vec<PackageSurfaceRow>, SqliteInputError> {
    if !table_exists(connection, "package_surfaces")? {
        return Ok(Vec::new());
    }

    let mut statement = connection.prepare(
        r"
        SELECT
            project_id,
            package_name,
            package_version,
            export_specifier,
            status,
            evidence_json
        FROM package_surfaces
        WHERE project_id = ?1
        ORDER BY package_name, export_specifier
        ",
    )?;
    let rows = statement.query_map(params![i64::from(project_id)], |row| {
        let status = attribution_status_from_database(row.get::<_, String>(4)?.as_str())?;
        Ok(PackageSurfaceRow {
            project_id: row.get(0)?,
            package_name: row.get(1)?,
            package_version: row.get(2)?,
            export_specifier: row.get(3)?,
            status,
            evidence: row.get(5)?,
        })
    })?;

    collect_sqlite_rows(rows)
}

fn load_project_assets(
    connection: &Connection,
    project_id: u32,
) -> Result<Vec<AssetRow>, SqliteInputError> {
    if !table_exists(connection, "project_assets")? {
        return Ok(Vec::new());
    }

    let mut statement = connection.prepare(
        r"
        SELECT
            id,
            project_id,
            logical_path,
            output_path,
            source_path,
            kind,
            executable,
            platform,
            arch
        FROM project_assets
        WHERE project_id = ?1
        ORDER BY id
        ",
    )?;
    let rows = statement.query_map(params![i64::from(project_id)], |row| {
        let source_path = row.get::<_, String>(4)?;
        let bytes = fs::read(source_path.as_str()).map_err(|source| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(ReadSourceError {
                path: PathBuf::from(source_path.clone()),
                source,
            }))
        })?;
        Ok(AssetRow {
            id: row.get(0)?,
            project_id: row.get(1)?,
            logical_path: row.get(2)?,
            output_path: row.get(3)?,
            source_path: Some(source_path),
            bytes,
            kind: row.get(5)?,
            executable: row.get::<_, i64>(6)? != 0,
            platform: row.get(7)?,
            arch: row.get(8)?,
        })
    })?;

    collect_sqlite_rows(rows)
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool, SqliteInputError> {
    let exists = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        params![table],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(exists == 1)
}

fn collect_sqlite_rows<T>(
    rows: impl IntoIterator<Item = rusqlite::Result<T>>,
) -> Result<Vec<T>, SqliteInputError> {
    let mut output = Vec::new();
    for row in rows {
        output.push(row?);
    }
    Ok(output)
}

fn module_kind_from_category(category: Option<&str>) -> StoredModuleKind {
    match category.map(str::trim) {
        Some("package") => StoredModuleKind::Package,
        Some("builtin") => StoredModuleKind::Builtin,
        Some("application") | Some("unknown") | Some("") | None => StoredModuleKind::Application,
        Some(_) => StoredModuleKind::Application,
    }
}

fn module_semantic_path(id: i64, semantic_name: Option<&str>, original_name: &str) -> String {
    let seed = semantic_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(original_name);
    let slug = path_slug(seed);
    format!("modules/{id}-{slug}.ts")
}

fn path_slug(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut last_was_separator = false;

    for ch in value.chars() {
        let mapped = if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '/') {
            ch
        } else {
            '-'
        };

        if mapped == '-' {
            if last_was_separator {
                continue;
            }
            last_was_separator = true;
        } else {
            last_was_separator = false;
        }
        output.push(mapped);
    }

    let trimmed = output.trim_matches(|ch| matches!(ch, '-' | '/' | '.'));
    if trimmed.is_empty() {
        "module".to_string()
    } else {
        trimmed.to_string()
    }
}

fn emission_mode_from_database(value: &str) -> rusqlite::Result<PackageEmissionMode> {
    match value {
        "external_import" => Ok(PackageEmissionMode::ExternalImport),
        "vendored_asset" => Ok(PackageEmissionMode::VendoredAsset),
        "application_source" => Ok(PackageEmissionMode::ApplicationSource),
        "runtime_glue" => Ok(PackageEmissionMode::RuntimeGlue),
        _ => Err(rusqlite::Error::InvalidParameterName(format!(
            "unknown package emission mode {value}"
        ))),
    }
}

fn attribution_status_from_database(value: &str) -> rusqlite::Result<PackageAttributionStatus> {
    match value {
        "proposed" => Ok(PackageAttributionStatus::Proposed),
        "accepted" => Ok(PackageAttributionStatus::Accepted),
        "rejected" => Ok(PackageAttributionStatus::Rejected),
        _ => Err(rusqlite::Error::InvalidParameterName(format!(
            "unknown package attribution status {value}"
        ))),
    }
}

#[derive(Debug)]
pub enum SqliteInputError {
    OpenDatabase {
        path: PathBuf,
        source: rusqlite::Error,
    },
    ProjectNotFound {
        project_id: u32,
    },
    Sqlite(rusqlite::Error),
    InputBundle(InputBundleError),
}

impl fmt::Display for SqliteInputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpenDatabase { path, source } => {
                write!(
                    formatter,
                    "failed to open SQLite database {}: {source}",
                    path.display()
                )
            }
            Self::ProjectNotFound { project_id } => {
                write!(
                    formatter,
                    "project {project_id} was not found in SQLite database"
                )
            }
            Self::Sqlite(source) => write!(formatter, "SQLite query failed: {source}"),
            Self::InputBundle(source) => {
                write!(formatter, "SQLite rows are not valid input: {source}")
            }
        }
    }
}

impl Error for SqliteInputError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::OpenDatabase { source, .. } | Self::Sqlite(source) => Some(source),
            Self::InputBundle(source) => Some(source),
            Self::ProjectNotFound { .. } => None,
        }
    }
}

impl From<rusqlite::Error> for SqliteInputError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Sqlite(source)
    }
}

#[derive(Debug)]
struct ReadSourceError {
    path: PathBuf,
    source: std::io::Error,
}

impl fmt::Display for ReadSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "failed to read {}: {}",
            self.path.display(),
            self.source
        )
    }
}

impl Error for ReadSourceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::Connection;
    use tempfile::tempdir;

    use reverts_ir::{ModuleId, ModuleKind};

    use crate::sqlite::{load_project_bundle_from_connection, load_project_rows_from_connection};
    use crate::{AssetKind, InputBundleError, ModuleDependencyTarget, PackageAttributionStatus};

    #[test]
    fn sqlite_project_loader_builds_valid_bundle_without_live_database() {
        let connection = Connection::open_in_memory().expect("in-memory database should open");
        create_schema(&connection);
        let tempdir = tempdir().expect("tempdir should be created");
        let source_path = tempdir.path().join("bundle.js");
        fs::write(&source_path, "export const activate = 42;")
            .expect("fixture source should be written");
        insert_fixture_project(&connection, source_path.to_string_lossy().as_ref());
        let asset_path = tempdir.path().join("vendor").join("rg");
        fs::create_dir_all(asset_path.parent().expect("asset parent"))
            .expect("fixture asset parent should be created");
        fs::write(asset_path.as_path(), b"rg-binary").expect("fixture asset should be written");
        connection
            .execute(
                r"
                INSERT INTO project_assets
                    (id, project_id, logical_path, output_path, source_path, kind, executable, platform, arch)
                VALUES (501, 7, '/$bunfs/root/vendor/rg', 'modules/10-entry/vendor/rg', ?1, 'executable', 1, 'linux', 'x64')
                ",
                [asset_path.to_string_lossy().as_ref()],
            )
            .expect("fixture asset row should be inserted");

        let bundle = load_project_bundle_from_connection(&connection, 7)
            .expect("fixture database should load");

        assert_eq!(bundle.project.name, "fixture-project");
        assert_eq!(bundle.source_files.len(), 1);
        assert_eq!(bundle.modules.len(), 2);
        assert_eq!(bundle.modules[0].kind, ModuleKind::Application);
        assert_eq!(bundle.modules[0].semantic_path, "modules/10-entry/main.ts");
        assert_eq!(bundle.modules[1].kind, ModuleKind::Package);
        assert_eq!(bundle.symbols.len(), 1);
        assert_eq!(bundle.symbols[0].name, "activate");
        assert_eq!(
            bundle.symbols[0].semantic_name.as_deref(),
            Some("runActivation")
        );
        assert!(matches!(
            bundle.dependencies[0].target,
            ModuleDependencyTarget::Module(ModuleId(11))
        ));
        assert_eq!(
            bundle.package_attributions[0].status,
            PackageAttributionStatus::Accepted
        );
        assert_eq!(bundle.package_surfaces.len(), 1);
        assert_eq!(bundle.package_surfaces[0].package_name, "undici");
        assert_eq!(
            bundle.package_surfaces[0].package_version.as_deref(),
            Some("2.2.1")
        );
        assert_eq!(bundle.package_surfaces[0].export_specifier, "undici");
        assert_eq!(
            bundle.source_files[0].source.as_deref(),
            Some("export const activate = 42;")
        );
        assert_eq!(bundle.assets.len(), 1);
        assert_eq!(bundle.assets[0].id, 501);
        assert_eq!(
            bundle.assets[0].logical_path.as_str(),
            "/$bunfs/root/vendor/rg"
        );
        assert_eq!(
            bundle.assets[0].output_path.as_str(),
            "modules/10-entry/vendor/rg"
        );
        assert_eq!(bundle.assets[0].bytes, b"rg-binary");
        assert_eq!(bundle.assets[0].kind, AssetKind::Executable);
        assert!(bundle.assets[0].executable);
    }

    #[test]
    fn sqlite_project_loader_reports_missing_project() {
        let connection = Connection::open_in_memory().expect("in-memory database should open");
        create_schema(&connection);

        let error = load_project_bundle_from_connection(&connection, 404);

        assert!(error.is_err());
    }

    #[test]
    fn sqlite_project_loader_treats_missing_project_assets_table_as_empty() {
        let connection = Connection::open_in_memory().expect("in-memory database should open");
        create_schema_without_project_assets(&connection);
        let tempdir = tempdir().expect("tempdir should be created");
        let source_path = tempdir.path().join("bundle.js");
        fs::write(&source_path, "export const activate = 42;")
            .expect("fixture source should be written");
        insert_fixture_project(&connection, source_path.to_string_lossy().as_ref());

        let bundle = load_project_bundle_from_connection(&connection, 7)
            .expect("fixture database should load without project_assets");

        assert!(bundle.assets.is_empty());
    }

    #[test]
    fn sqlite_rows_can_load_before_package_attribution_contract_is_complete() {
        let connection = Connection::open_in_memory().expect("in-memory database should open");
        create_schema(&connection);
        let tempdir = tempdir().expect("tempdir should be created");
        let source_path = tempdir.path().join("bundle.js");
        fs::write(&source_path, "export const activate = 42;")
            .expect("fixture source should be written");
        insert_fixture_project(&connection, source_path.to_string_lossy().as_ref());
        connection
            .execute("DELETE FROM package_attributions WHERE module_id = 11", [])
            .expect("fixture attribution should be removed");

        let rows = load_project_rows_from_connection(&connection, 7)
            .expect("rows should load for matching before bundle validation");
        let bundle_error = load_project_bundle_from_connection(&connection, 7);

        assert_eq!(rows.modules.len(), 2);
        assert!(matches!(
            bundle_error,
            Err(super::SqliteInputError::InputBundle(
                InputBundleError::MissingPackageAttribution {
                    module_id: ModuleId(11)
                }
            ))
        ));
    }

    fn create_schema(connection: &Connection) {
        create_schema_without_project_assets(connection);
        connection
            .execute_batch(
                r"
                CREATE TABLE project_assets (
                    id INTEGER PRIMARY KEY,
                    project_id INTEGER NOT NULL,
                    logical_path TEXT NOT NULL,
                    output_path TEXT NOT NULL,
                    source_path TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    executable INTEGER NOT NULL DEFAULT 0,
                    platform TEXT,
                    arch TEXT
                );
                ",
            )
            .expect("fixture asset schema should be created");
    }

    fn create_schema_without_project_assets(connection: &Connection) {
        connection
            .execute_batch(
                r"
                CREATE TABLE projects (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                CREATE TABLE source_files (id INTEGER PRIMARY KEY, file_path TEXT NOT NULL);
                CREATE TABLE project_files (project_id INTEGER NOT NULL, file_id INTEGER NOT NULL);
                CREATE TABLE modules (
                    id INTEGER PRIMARY KEY,
                    file_id INTEGER,
                    original_name TEXT NOT NULL,
                    semantic_name TEXT,
                    module_category TEXT,
                    package_name TEXT,
                    package_version TEXT,
                    byte_start INTEGER,
                    byte_end INTEGER
                );
                CREATE TABLE symbols (
                    id INTEGER PRIMARY KEY,
                    module_id INTEGER,
                    original_name TEXT NOT NULL,
                    semantic_name TEXT,
                    export_name TEXT,
                    scope_level TEXT NOT NULL
                );
                CREATE TABLE module_dependencies (
                    module_id INTEGER NOT NULL,
                    dependency_id INTEGER NOT NULL
                );
                CREATE TABLE package_attributions (
                    module_id INTEGER NOT NULL,
                    package_name TEXT NOT NULL,
                    package_version TEXT,
                    package_subpath TEXT,
                    export_specifier TEXT,
                    emission_mode TEXT NOT NULL,
                    status TEXT NOT NULL,
                    rejection_reason TEXT
                );
                CREATE TABLE package_surfaces (
                    project_id INTEGER NOT NULL,
                    package_name TEXT NOT NULL,
                    package_version TEXT,
                    export_specifier TEXT NOT NULL,
                    status TEXT NOT NULL,
                    evidence_json TEXT
                );
                ",
            )
            .expect("fixture schema should be created");
    }

    fn insert_fixture_project(connection: &Connection, source_path: &str) {
        let sql = format!(
            r"
                INSERT INTO projects (id, name) VALUES (7, 'fixture-project');
                INSERT INTO source_files (id, file_path) VALUES (101, '{}');
                INSERT INTO project_files (project_id, file_id) VALUES (7, 101);
                INSERT INTO modules
                    (id, file_id, original_name, semantic_name, module_category, package_name, package_version, byte_start, byte_end)
                    VALUES
                    (10, 101, 'app', 'entry/main', 'application', NULL, NULL, 0, 27),
                    (11, 101, 'lodash_map', 'lodash/map', 'package', 'lodash', '4.17.21', 0, 0);
                INSERT INTO symbols
                    (id, module_id, original_name, semantic_name, export_name, scope_level)
                    VALUES
                    (1001, 10, 'activate', 'runActivation', NULL, 'module'),
                    (1002, 10, 'activate', 'runActivation', NULL, 'module'),
                    (1003, 10, 'inner', 'inner', NULL, 'local');
                INSERT INTO module_dependencies (module_id, dependency_id) VALUES (10, 11);
                INSERT INTO package_attributions
                    (module_id, package_name, package_version, package_subpath, export_specifier, emission_mode, status, rejection_reason)
                    VALUES
                    (11, 'lodash', '4.17.21', 'map', 'lodash/map', 'external_import', 'accepted', NULL);
                INSERT INTO package_surfaces
                    (project_id, package_name, package_version, export_specifier, status, evidence_json)
                    VALUES
                    (7, 'undici', '2.2.1', 'undici', 'accepted', NULL);
                ",
            source_path.replace('\'', "''")
        );
        connection
            .execute_batch(sql.as_str())
            .expect("fixture rows should be inserted");
    }
}
