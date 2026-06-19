//! `import-unpacked` subcommand: bridge unpack Skill evidence into the
//! canonical SQLite facts consumed by `InputBundle`.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use reverts_js::{ParseGoal, collect_static_module_specifiers};
use rusqlite::{Connection, params};
use serde_json::Value;

use crate::args::ImportUnpackedArgs;
use crate::errors::CliRunError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportUnpackedOutcome {
    pub project_id: u32,
    pub source_files: usize,
    pub modules: usize,
    pub module_dependencies: usize,
    pub assets: usize,
    pub native_assets: usize,
    pub deferred_sources: usize,
    pub bundle_sources: usize,
    pub package_attributions: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImportFile {
    relative_path: String,
    physical_path: PathBuf,
    kind: ImportFileKind,
    package: Option<PackageOwner>,
    executable: bool,
    deferred_source: bool,
    bundle_source: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportFileKind {
    Source,
    Asset,
    NativeAsset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PackageOwner {
    package_name: String,
    package_version: Option<String>,
    package_root: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceRecord {
    id: u32,
    relative_path: String,
    physical_path: PathBuf,
    stored_path: PathBuf,
    package: Option<PackageOwner>,
    bundle_source: bool,
}

pub(crate) fn run(args: ImportUnpackedArgs) -> Result<(), CliRunError> {
    let outcome = import_unpacked_to_sqlite(&args).map_err(CliRunError::ImportUnpacked)?;
    println!(
        "imported unpacked target as project {} into {}: {} source file(s), {} module(s), {} module edge(s), {} asset(s), {} native asset(s), {} deferred source asset(s), {} bundle source file(s), {} package attribution(s)",
        outcome.project_id,
        args.output_db.display(),
        outcome.source_files,
        outcome.modules,
        outcome.module_dependencies,
        outcome.assets,
        outcome.native_assets,
        outcome.deferred_sources,
        outcome.bundle_sources,
        outcome.package_attributions
    );
    Ok(())
}

pub fn import_unpacked_to_sqlite(
    args: &ImportUnpackedArgs,
) -> Result<ImportUnpackedOutcome, ImportUnpackedError> {
    let input_root =
        args.input
            .canonicalize()
            .map_err(|source| ImportUnpackedError::ReadInputRoot {
                path: args.input.clone(),
                source,
            })?;
    if !input_root.is_dir() {
        return Err(ImportUnpackedError::InputRootNotDirectory(input_root));
    }
    read_manifest(args.manifest.as_path())?;
    if args.output_db.exists() {
        return Err(ImportUnpackedError::OutputDatabaseExists {
            path: args.output_db.clone(),
        });
    }
    if let Some(parent) = args.output_db.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|source| ImportUnpackedError::CreateOutputParent {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let files = collect_import_files(
        input_root.as_path(),
        args.ignore_native_assets,
        args.max_source_bytes,
        args.bundle_source_bytes,
    )?;
    let sources = materialize_source_records(&files, args)?;
    let module_by_path = sources
        .iter()
        .filter(|source| !source.bundle_source)
        .map(|source| (source.relative_path.clone(), source.id))
        .collect::<BTreeMap<_, _>>();
    let dependencies = collect_module_dependencies(&sources, &module_by_path);

    let mut connection = Connection::open(args.output_db.as_path()).map_err(|source| {
        ImportUnpackedError::OpenOutputDatabase {
            path: args.output_db.clone(),
            source,
        }
    })?;
    connection
        .execute_batch("PRAGMA foreign_keys = ON")
        .map_err(ImportUnpackedError::ConfigureDatabase)?;
    create_schema(&connection).map_err(ImportUnpackedError::WriteDatabase)?;
    persist_import(&mut connection, args, &files, &sources, &dependencies)?;

    let assets = files
        .iter()
        .filter(|file| file.kind == ImportFileKind::Asset)
        .count();
    let native_assets = files
        .iter()
        .filter(|file| file.kind == ImportFileKind::NativeAsset)
        .count();
    let deferred_sources = files.iter().filter(|file| file.deferred_source).count();
    let bundle_sources = sources.iter().filter(|source| source.bundle_source).count();
    let modules = sources.len() - bundle_sources;
    let package_attributions = sources
        .iter()
        .filter(|source| !source.bundle_source && source.package.is_some())
        .count();
    Ok(ImportUnpackedOutcome {
        project_id: 1,
        source_files: sources.len(),
        modules,
        module_dependencies: dependencies.len(),
        assets,
        native_assets,
        deferred_sources,
        bundle_sources,
        package_attributions,
    })
}

fn read_manifest(path: &Path) -> Result<Value, ImportUnpackedError> {
    let content = fs::read_to_string(path).map_err(|source| ImportUnpackedError::ReadManifest {
        path: path.to_path_buf(),
        source,
    })?;
    let value = serde_json::from_str::<Value>(content.as_str()).map_err(|source| {
        ImportUnpackedError::ParseManifest {
            path: path.to_path_buf(),
            source,
        }
    })?;
    let is_supported = value
        .get("schema")
        .and_then(Value::as_str)
        .is_some_and(|schema| schema == "reverts.import_evidence.v1")
        || value.get("reverts_import_evidence").is_some()
        || value.get("target_kind").and_then(Value::as_str) == Some("electron_app")
        || (value.get("asar_meta").is_some() && value.get("entries").is_some());
    if is_supported {
        Ok(value)
    } else {
        Err(ImportUnpackedError::UnsupportedManifest {
            path: path.to_path_buf(),
        })
    }
}

fn collect_import_files(
    input_root: &Path,
    ignore_native_assets: bool,
    max_source_bytes: Option<u64>,
    bundle_source_bytes: Option<u64>,
) -> Result<Vec<ImportFile>, ImportUnpackedError> {
    let mut paths = Vec::new();
    collect_file_paths(input_root, &mut paths)?;
    paths.sort();

    let mut files = Vec::new();
    for physical_path in paths {
        let relative_path = relative_path(input_root, physical_path.as_path())?;
        let mut kind = classify_file(relative_path.as_str(), physical_path.as_path());
        let source_size = if kind == ImportFileKind::Source {
            file_size(physical_path.as_path())
        } else {
            None
        };
        if ignore_native_assets && kind == ImportFileKind::NativeAsset {
            continue;
        }
        let package = package_owner(relative_path.as_str(), input_root)?;
        let bundle_source = package.is_none()
            && source_size
                .is_some_and(|size| bundle_source_bytes.is_some_and(|limit| size > limit));
        let deferred_source = !bundle_source
            && source_size.is_some_and(|size| max_source_bytes.is_some_and(|limit| size > limit));
        if deferred_source {
            kind = ImportFileKind::Asset;
        }
        files.push(ImportFile {
            relative_path,
            physical_path: physical_path.clone(),
            kind,
            package,
            executable: is_executable(physical_path.as_path()),
            deferred_source,
            bundle_source,
        });
    }
    Ok(files)
}

fn file_size(path: &Path) -> Option<u64> {
    fs::metadata(path).map(|metadata| metadata.len()).ok()
}

fn collect_file_paths(root: &Path, output: &mut Vec<PathBuf>) -> Result<(), ImportUnpackedError> {
    for entry in fs::read_dir(root).map_err(|source| ImportUnpackedError::ReadDirectory {
        path: root.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| ImportUnpackedError::ReadDirectory {
            path: root.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|source| ImportUnpackedError::ReadMetadata {
                path: path.clone(),
                source,
            })?;
        if file_type.is_dir() {
            collect_file_paths(path.as_path(), output)?;
        } else if file_type.is_file() {
            output.push(path);
        }
    }
    Ok(())
}

fn relative_path(root: &Path, path: &Path) -> Result<String, ImportUnpackedError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_source| ImportUnpackedError::InvalidPath {
            path: path.to_path_buf(),
        })?;
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(value) => parts.push(value.to_string_lossy().into_owned()),
            _ => {
                return Err(ImportUnpackedError::InvalidPath {
                    path: path.to_path_buf(),
                });
            }
        }
    }
    Ok(parts.join("/"))
}

fn classify_file(relative_path: &str, physical_path: &Path) -> ImportFileKind {
    let extension = Path::new(relative_path)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_ascii_lowercase);
    if matches!(
        extension.as_deref(),
        Some("js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" | "mts" | "cts")
    ) {
        return ImportFileKind::Source;
    }
    if matches!(
        extension.as_deref(),
        Some("node" | "dylib" | "so" | "dll" | "exe")
    ) || Path::new(relative_path)
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        == Some("spawn-helper")
    {
        return ImportFileKind::NativeAsset;
    }
    if is_probably_macho(physical_path) {
        return ImportFileKind::NativeAsset;
    }
    ImportFileKind::Asset
}

fn is_probably_macho(path: &Path) -> bool {
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    matches!(
        bytes.get(0..4),
        Some(
            b"\xfe\xed\xfa\xce"
                | b"\xce\xfa\xed\xfe"
                | b"\xfe\xed\xfa\xcf"
                | b"\xcf\xfa\xed\xfe"
                | b"\xca\xfe\xba\xbe"
                | b"\xbe\xba\xfe\xca"
                | b"\xca\xfe\xba\xbf"
                | b"\xbf\xba\xfe\xca"
        )
    )
}

fn package_owner(
    relative_path: &str,
    input_root: &Path,
) -> Result<Option<PackageOwner>, ImportUnpackedError> {
    let parts = relative_path.split('/').collect::<Vec<_>>();
    if parts.len() < 2 || parts.first() != Some(&"node_modules") {
        return Ok(None);
    }
    let (package_name, package_root) = if parts.get(1).is_some_and(|part| part.starts_with('@')) {
        let (Some(scope), Some(name)) = (parts.get(1), parts.get(2)) else {
            return Ok(None);
        };
        (format!("{scope}/{name}"), format!("{scope}/{name}"))
    } else {
        let Some(name) = parts.get(1) else {
            return Ok(None);
        };
        ((*name).to_string(), (*name).to_string())
    };
    let package_root = format!("node_modules/{package_root}");
    let package_version = read_package_version(input_root.join(&package_root).as_path())?;
    Ok(Some(PackageOwner {
        package_name,
        package_version,
        package_root,
    }))
}

fn read_package_version(package_root: &Path) -> Result<Option<String>, ImportUnpackedError> {
    let package_json = package_root.join("package.json");
    if !package_json.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(package_json.as_path()).map_err(|source| {
        ImportUnpackedError::ReadPackageJson {
            path: package_json.clone(),
            source,
        }
    })?;
    let value = serde_json::from_str::<Value>(content.as_str()).map_err(|source| {
        ImportUnpackedError::ParsePackageJson {
            path: package_json,
            source,
        }
    })?;
    Ok(value
        .get("version")
        .and_then(Value::as_str)
        .map(ToString::to_string))
}

fn materialize_source_records(
    files: &[ImportFile],
    args: &ImportUnpackedArgs,
) -> Result<Vec<SourceRecord>, ImportUnpackedError> {
    let mut sources = Vec::new();
    for file in files
        .iter()
        .filter(|file| file.kind == ImportFileKind::Source)
    {
        let id = u32::try_from(sources.len() + 1).map_err(|_source| {
            ImportUnpackedError::TooManyFiles {
                count: sources.len() + 1,
            }
        })?;
        let stored_path = materialize_parseable_source(file, args)?;
        sources.push(SourceRecord {
            id,
            relative_path: file.relative_path.clone(),
            physical_path: file.physical_path.clone(),
            stored_path,
            package: file.package.clone(),
            bundle_source: file.bundle_source,
        });
    }
    if sources.is_empty() {
        return Err(ImportUnpackedError::NoSourceFiles {
            input: args.input.clone(),
        });
    }
    Ok(sources)
}

fn materialize_parseable_source(
    file: &ImportFile,
    args: &ImportUnpackedArgs,
) -> Result<PathBuf, ImportUnpackedError> {
    let content = fs::read_to_string(file.physical_path.as_path()).map_err(|source| {
        ImportUnpackedError::ReadSource {
            path: file.physical_path.clone(),
            source,
        }
    })?;
    if !content.starts_with("#!") {
        return Ok(file.physical_path.clone());
    }
    let parent = args
        .output_db
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let normalized_path = parent
        .join(".reverts-import-sources")
        .join(sanitize_path_segment(args.project_name.as_str()))
        .join(Path::new(file.relative_path.as_str()));
    if let Some(parent) = normalized_path.parent() {
        fs::create_dir_all(parent).map_err(|source| ImportUnpackedError::WriteSource {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let normalized = format!(
        "//{}",
        content.strip_prefix("#").unwrap_or(content.as_str())
    );
    fs::write(normalized_path.as_path(), normalized).map_err(|source| {
        ImportUnpackedError::WriteSource {
            path: normalized_path.clone(),
            source,
        }
    })?;
    Ok(normalized_path)
}

fn collect_module_dependencies(
    sources: &[SourceRecord],
    module_by_path: &BTreeMap<String, u32>,
) -> BTreeSet<(u32, u32)> {
    let mut dependencies = BTreeSet::new();
    for source in sources {
        if source.bundle_source {
            continue;
        }
        let Ok(content) = fs::read_to_string(source.stored_path.as_path()) else {
            continue;
        };
        let Ok(specifiers) = collect_static_module_specifiers(
            content.as_str(),
            Some(source.stored_path.as_path()),
            ParseGoal::TypeScript,
        ) else {
            continue;
        };
        for specifier in specifiers {
            if let Some(target) = resolve_local_specifier(
                source.relative_path.as_str(),
                specifier.value.as_str(),
                module_by_path,
            ) {
                dependencies.insert((source.id, target));
            }
        }
    }
    dependencies
}

fn resolve_local_specifier(
    from_path: &str,
    specifier: &str,
    module_by_path: &BTreeMap<String, u32>,
) -> Option<u32> {
    if !(specifier.starts_with("./") || specifier.starts_with("../")) {
        return None;
    }
    let base = normalize_relative_specifier(from_path, specifier)?;
    let mut candidates = vec![base.clone()];
    for extension in ["js", "mjs", "cjs", "jsx", "ts", "tsx", "mts", "cts"] {
        candidates.push(format!("{base}.{extension}"));
    }
    for extension in ["js", "mjs", "cjs", "jsx", "ts", "tsx", "mts", "cts"] {
        candidates.push(format!("{base}/index.{extension}"));
    }
    candidates
        .iter()
        .find_map(|candidate| module_by_path.get(candidate).copied())
}

fn normalize_relative_specifier(from_path: &str, specifier: &str) -> Option<String> {
    let mut parts = from_path.split('/').collect::<Vec<_>>();
    parts.pop();
    for segment in specifier.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            value => parts.push(value),
        }
    }
    Some(parts.join("/"))
}

fn create_schema(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
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
            byte_end INTEGER,
            created_at TEXT,
            updated_at TEXT
        );
        CREATE TABLE symbols (
            module_id INTEGER,
            semantic_name TEXT,
            export_name TEXT,
            original_name TEXT,
            scope_level TEXT
        );
        CREATE TABLE module_dependencies (module_id INTEGER, dependency_id INTEGER);
        CREATE TABLE package_attributions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            module_id INTEGER NOT NULL,
            module_original_name TEXT NOT NULL,
            package_name TEXT NOT NULL,
            package_version TEXT,
            package_subpath TEXT,
            resolved_file TEXT,
            export_specifier TEXT,
            emission_mode TEXT NOT NULL,
            status TEXT NOT NULL,
            evidence_json TEXT,
            rejection_reason TEXT,
            external_import_policy_version INTEGER,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            UNIQUE (module_id)
        );
        CREATE TABLE package_surfaces (
            project_id INTEGER NOT NULL,
            package_name TEXT NOT NULL,
            package_version TEXT NOT NULL,
            export_specifier TEXT NOT NULL,
            status TEXT NOT NULL,
            evidence_json TEXT
        );
        CREATE TABLE project_assets (
            id INTEGER PRIMARY KEY,
            project_id INTEGER NOT NULL,
            logical_path TEXT NOT NULL,
            output_path TEXT NOT NULL,
            source_path TEXT NOT NULL,
            kind TEXT NOT NULL,
            executable INTEGER NOT NULL,
            platform TEXT,
            arch TEXT
        );
        ",
    )
}

fn persist_import(
    connection: &mut Connection,
    args: &ImportUnpackedArgs,
    files: &[ImportFile],
    sources: &[SourceRecord],
    dependencies: &BTreeSet<(u32, u32)>,
) -> Result<(), ImportUnpackedError> {
    let transaction = connection
        .transaction()
        .map_err(ImportUnpackedError::WriteDatabase)?;
    transaction
        .execute(
            "INSERT INTO projects (id, name) VALUES (1, ?1)",
            params![args.project_name],
        )
        .map_err(ImportUnpackedError::WriteDatabase)?;

    for source in sources {
        transaction
            .execute(
                "INSERT INTO source_files (id, file_path) VALUES (?1, ?2)",
                params![source.id, source.stored_path.to_string_lossy()],
            )
            .map_err(ImportUnpackedError::WriteDatabase)?;
        transaction
            .execute(
                "INSERT INTO project_files (project_id, file_id) VALUES (1, ?1)",
                params![source.id],
            )
            .map_err(ImportUnpackedError::WriteDatabase)?;
        if source.bundle_source {
            continue;
        }
        let module_category = if source.package.is_some() {
            "package"
        } else {
            "application"
        };
        let (package_name, package_version) = source
            .package
            .as_ref()
            .map(|package| {
                (
                    Some(package.package_name.as_str()),
                    package.package_version.as_deref(),
                )
            })
            .unwrap_or((None, None));
        transaction
            .execute(
                r"
                INSERT INTO modules
                    (id, file_id, original_name, semantic_name, module_category, package_name,
                     package_version, byte_start, byte_end, created_at, updated_at)
                VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, datetime('now'), datetime('now'))
                ",
                params![
                    source.id,
                    source.relative_path,
                    semantic_name(source.relative_path.as_str()),
                    module_category,
                    package_name,
                    package_version
                ],
            )
            .map_err(ImportUnpackedError::WriteDatabase)?;
        if let Some(package) = &source.package {
            let subpath = source
                .relative_path
                .strip_prefix(package.package_root.as_str())
                .map(|value| value.trim_start_matches('/'))
                .filter(|value| !value.is_empty());
            let evidence = serde_json::json!({
                "source": "import-unpacked",
                "target_kind": "electron_app",
                "package_root": package.package_root,
                "physical_path": source.physical_path,
                "manifest": args.manifest,
            })
            .to_string();
            transaction
                .execute(
                    r"
                    INSERT INTO package_attributions
                        (module_id, module_original_name, package_name, package_version,
                         package_subpath, resolved_file, export_specifier, emission_mode, status,
                         evidence_json, rejection_reason, external_import_policy_version,
                         created_at, updated_at)
                    VALUES
                        (?1, ?2, ?3, ?4, ?5, ?6, NULL, 'application_source', 'rejected',
                         ?7, 'package source preserved locally from unpacked target evidence',
                         NULL, datetime('now'), datetime('now'))
                    ",
                    params![
                        source.id,
                        source.relative_path,
                        package.package_name,
                        package.package_version,
                        subpath,
                        source.physical_path.to_string_lossy(),
                        evidence
                    ],
                )
                .map_err(ImportUnpackedError::WriteDatabase)?;
        }
    }

    for (module_id, dependency_id) in dependencies {
        transaction
            .execute(
                "INSERT INTO module_dependencies (module_id, dependency_id) VALUES (?1, ?2)",
                params![module_id, dependency_id],
            )
            .map_err(ImportUnpackedError::WriteDatabase)?;
    }

    let mut asset_id = 1_u32;
    for file in files
        .iter()
        .filter(|file| file.kind != ImportFileKind::Source)
    {
        transaction
            .execute(
                r"
                INSERT INTO project_assets
                    (id, project_id, logical_path, output_path, source_path, kind, executable,
                     platform, arch)
                VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6, NULL, NULL)
                ",
                params![
                    asset_id,
                    file.relative_path,
                    format!("assets/{}", file.relative_path),
                    file.physical_path.to_string_lossy(),
                    asset_kind(file),
                    if file.executable { 1_i64 } else { 0_i64 }
                ],
            )
            .map_err(ImportUnpackedError::WriteDatabase)?;
        asset_id = asset_id
            .checked_add(1)
            .ok_or(ImportUnpackedError::TooManyFiles { count: usize::MAX })?;
    }

    transaction
        .commit()
        .map_err(ImportUnpackedError::WriteDatabase)
}

fn semantic_name(relative_path: &str) -> String {
    Path::new(relative_path)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/")
}

fn asset_kind(file: &ImportFile) -> &'static str {
    if file.deferred_source {
        return "data";
    }
    if file.kind == ImportFileKind::NativeAsset {
        let extension = Path::new(file.relative_path.as_str())
            .extension()
            .and_then(std::ffi::OsStr::to_str);
        return if extension == Some("node") {
            "native_node"
        } else {
            "executable"
        };
    }
    match Path::new(file.relative_path.as_str())
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("wasm") => "wasm",
        Some("png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "avif" | "ico") => "image",
        Some("ttf" | "otf" | "woff" | "woff2") => "font",
        Some("css") => "css",
        Some("html") => "html",
        _ => "data",
    }
}

fn sanitize_path_segment(value: &str) -> String {
    let mut output = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            output.push(ch);
        } else if !output.ends_with('-') {
            output.push('-');
        }
    }
    let trimmed = output.trim_matches('-');
    if trimmed.is_empty() {
        "project".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    false
}

#[derive(Debug)]
pub enum ImportUnpackedError {
    ReadInputRoot {
        path: PathBuf,
        source: io::Error,
    },
    InputRootNotDirectory(PathBuf),
    ReadManifest {
        path: PathBuf,
        source: io::Error,
    },
    ParseManifest {
        path: PathBuf,
        source: serde_json::Error,
    },
    UnsupportedManifest {
        path: PathBuf,
    },
    OutputDatabaseExists {
        path: PathBuf,
    },
    CreateOutputParent {
        path: PathBuf,
        source: io::Error,
    },
    OpenOutputDatabase {
        path: PathBuf,
        source: rusqlite::Error,
    },
    ConfigureDatabase(rusqlite::Error),
    ReadDirectory {
        path: PathBuf,
        source: io::Error,
    },
    ReadMetadata {
        path: PathBuf,
        source: io::Error,
    },
    InvalidPath {
        path: PathBuf,
    },
    ReadPackageJson {
        path: PathBuf,
        source: io::Error,
    },
    ParsePackageJson {
        path: PathBuf,
        source: serde_json::Error,
    },
    ReadSource {
        path: PathBuf,
        source: io::Error,
    },
    WriteSource {
        path: PathBuf,
        source: io::Error,
    },
    NoSourceFiles {
        input: PathBuf,
    },
    TooManyFiles {
        count: usize,
    },
    WriteDatabase(rusqlite::Error),
}

impl fmt::Display for ImportUnpackedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadInputRoot { path, source } => {
                write!(
                    formatter,
                    "failed to read input root {}: {source}",
                    path.display()
                )
            }
            Self::InputRootNotDirectory(path) => {
                write!(
                    formatter,
                    "input root is not a directory: {}",
                    path.display()
                )
            }
            Self::ReadManifest { path, source } => {
                write!(
                    formatter,
                    "failed to read manifest {}: {source}",
                    path.display()
                )
            }
            Self::ParseManifest { path, source } => {
                write!(
                    formatter,
                    "failed to parse manifest {}: {source}",
                    path.display()
                )
            }
            Self::UnsupportedManifest { path } => {
                write!(
                    formatter,
                    "unsupported unpack manifest {}; expected reverts.import_evidence.v1 or Electron evidence",
                    path.display()
                )
            }
            Self::OutputDatabaseExists { path } => {
                write!(
                    formatter,
                    "output database already exists: {}",
                    path.display()
                )
            }
            Self::CreateOutputParent { path, source } => {
                write!(
                    formatter,
                    "failed to create output directory {}: {source}",
                    path.display()
                )
            }
            Self::OpenOutputDatabase { path, source } => {
                write!(
                    formatter,
                    "failed to open output database {}: {source}",
                    path.display()
                )
            }
            Self::ConfigureDatabase(source) => {
                write!(formatter, "failed to configure SQLite: {source}")
            }
            Self::ReadDirectory { path, source } => {
                write!(
                    formatter,
                    "failed to read directory {}: {source}",
                    path.display()
                )
            }
            Self::ReadMetadata { path, source } => {
                write!(
                    formatter,
                    "failed to read metadata {}: {source}",
                    path.display()
                )
            }
            Self::InvalidPath { path } => {
                write!(formatter, "invalid input path {}", path.display())
            }
            Self::ReadPackageJson { path, source } => {
                write!(
                    formatter,
                    "failed to read package.json {}: {source}",
                    path.display()
                )
            }
            Self::ParsePackageJson { path, source } => {
                write!(
                    formatter,
                    "failed to parse package.json {}: {source}",
                    path.display()
                )
            }
            Self::ReadSource { path, source } => {
                write!(
                    formatter,
                    "failed to read source {}: {source}",
                    path.display()
                )
            }
            Self::WriteSource { path, source } => {
                write!(
                    formatter,
                    "failed to write normalized source {}: {source}",
                    path.display()
                )
            }
            Self::NoSourceFiles { input } => {
                write!(formatter, "no source files found under {}", input.display())
            }
            Self::TooManyFiles { count } => {
                write!(formatter, "too many import files: {count}")
            }
            Self::WriteDatabase(source) => write!(formatter, "failed to write SQLite: {source}"),
        }
    }
}

impl Error for ImportUnpackedError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadInputRoot { source, .. }
            | Self::ReadManifest { source, .. }
            | Self::CreateOutputParent { source, .. }
            | Self::ReadDirectory { source, .. }
            | Self::ReadMetadata { source, .. }
            | Self::ReadPackageJson { source, .. }
            | Self::ReadSource { source, .. }
            | Self::WriteSource { source, .. } => Some(source),
            Self::ParseManifest { source, .. } | Self::ParsePackageJson { source, .. } => {
                Some(source)
            }
            Self::OpenOutputDatabase { source, .. }
            | Self::ConfigureDatabase(source)
            | Self::WriteDatabase(source) => Some(source),
            Self::InputRootNotDirectory(_)
            | Self::UnsupportedManifest { .. }
            | Self::OutputDatabaseExists { .. }
            | Self::InvalidPath { .. }
            | Self::NoSourceFiles { .. }
            | Self::TooManyFiles { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn import_unpacked_writes_sources_edges_assets_and_package_ownership()
    -> Result<(), Box<dyn Error>> {
        let temp = tempdir()?;
        let root = temp.path().join("app");
        fs::create_dir_all(root.join("node_modules/ws"))?;
        fs::write(root.join("a.js"), "const b = require('./b');\n")?;
        fs::write(root.join("b.js"), "export const value = 1;\n")?;
        fs::write(
            root.join("node_modules/ws/package.json"),
            r#"{"name":"ws","version":"8.0.0"}"#,
        )?;
        fs::write(
            root.join("node_modules/ws/index.js"),
            "module.exports = {};\n",
        )?;
        fs::write(root.join("style.css"), "body{}\n")?;
        fs::write(root.join("native.node"), b"\xcf\xfa\xed\xfe")?;
        let manifest = temp.path().join("reverts-import-evidence.json");
        fs::write(
            manifest.as_path(),
            r#"{"schema":"reverts.import_evidence.v1","target_kind":"electron_app"}"#,
        )?;
        let output_db = temp.path().join("project.sqlite");
        let args = ImportUnpackedArgs {
            input: root,
            manifest,
            project_name: "fixture".to_string(),
            output_db: output_db.clone(),
            ignore_native_assets: true,
            max_source_bytes: None,
            bundle_source_bytes: None,
        };

        let outcome = import_unpacked_to_sqlite(&args)?;

        assert_eq!(outcome.source_files, 3);
        assert_eq!(outcome.modules, 3);
        assert_eq!(outcome.module_dependencies, 1);
        assert_eq!(outcome.assets, 2);
        assert_eq!(outcome.native_assets, 0);
        assert_eq!(outcome.bundle_sources, 0);
        assert_eq!(outcome.package_attributions, 1);

        let connection = Connection::open(output_db.as_path())?;
        let package_modules = connection.query_row(
            "SELECT COUNT(*) FROM modules WHERE module_category = 'package'",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let dependency_edges =
            connection.query_row("SELECT COUNT(*) FROM module_dependencies", [], |row| {
                row.get::<_, i64>(0)
            })?;
        let assets = connection.query_row("SELECT COUNT(*) FROM project_assets", [], |row| {
            row.get::<_, i64>(0)
        })?;
        assert_eq!(package_modules, 1);
        assert_eq!(dependency_edges, 1);
        assert_eq!(assets, 2);
        let bundle =
            reverts_input::sqlite::load_project_bundle_from_sqlite(output_db.as_path(), 1)?;
        assert_eq!(bundle.source_files.len(), 3);
        assert_eq!(bundle.modules.len(), 3);
        assert_eq!(bundle.dependencies.len(), 1);
        assert_eq!(bundle.assets.len(), 2);
        assert_eq!(bundle.package_attributions.len(), 1);
        Ok(())
    }

    #[test]
    fn import_unpacked_defers_sources_larger_than_budget() -> Result<(), Box<dyn Error>> {
        let temp = tempdir()?;
        let root = temp.path().join("app");
        fs::create_dir_all(root.as_path())?;
        fs::write(root.join("small.js"), "export const ok = true;\n")?;
        fs::write(root.join("large.js"), "export const large = 'too long';\n")?;
        let manifest = temp.path().join("reverts-import-evidence.json");
        fs::write(
            manifest.as_path(),
            r#"{"schema":"reverts.import_evidence.v1","target_kind":"electron_app"}"#,
        )?;
        let output_db = temp.path().join("project.sqlite");
        let args = ImportUnpackedArgs {
            input: root,
            manifest,
            project_name: "fixture".to_string(),
            output_db: output_db.clone(),
            ignore_native_assets: true,
            max_source_bytes: Some(24),
            bundle_source_bytes: None,
        };

        let outcome = import_unpacked_to_sqlite(&args)?;

        assert_eq!(outcome.source_files, 1);
        assert_eq!(outcome.modules, 1);
        assert_eq!(outcome.assets, 1);
        assert_eq!(outcome.deferred_sources, 1);
        assert_eq!(outcome.bundle_sources, 0);
        let connection = Connection::open(output_db.as_path())?;
        let asset_path =
            connection.query_row("SELECT logical_path FROM project_assets", [], |row| {
                row.get::<_, String>(0)
            })?;
        assert_eq!(asset_path, "large.js");
        let bundle =
            reverts_input::sqlite::load_project_bundle_from_sqlite(output_db.as_path(), 1)?;
        assert_eq!(bundle.source_files.len(), 1);
        assert_eq!(bundle.assets.len(), 1);
        Ok(())
    }

    #[test]
    fn import_unpacked_keeps_large_bundle_sources_for_pipeline_extraction()
    -> Result<(), Box<dyn Error>> {
        let temp = tempdir()?;
        let root = temp.path().join("app");
        fs::create_dir_all(root.as_path())?;
        fs::write(
            root.join("bundle.js"),
            "var __commonJS=(cb,mod)=>function(){return mod||(mod={exports:{}},cb(mod.exports,mod)),mod.exports};\nvar dep=__commonJS({\"src/dep.js\":(exports,module)=>{module.exports=1;}});\n",
        )?;
        let manifest = temp.path().join("reverts-import-evidence.json");
        fs::write(
            manifest.as_path(),
            r#"{"schema":"reverts.import_evidence.v1","target_kind":"electron_app"}"#,
        )?;
        let output_db = temp.path().join("project.sqlite");
        let args = ImportUnpackedArgs {
            input: root,
            manifest,
            project_name: "fixture".to_string(),
            output_db: output_db.clone(),
            ignore_native_assets: true,
            max_source_bytes: None,
            bundle_source_bytes: Some(1),
        };

        let outcome = import_unpacked_to_sqlite(&args)?;

        assert_eq!(outcome.source_files, 1);
        assert_eq!(outcome.modules, 0);
        assert_eq!(outcome.assets, 0);
        assert_eq!(outcome.deferred_sources, 0);
        assert_eq!(outcome.bundle_sources, 1);
        let connection = Connection::open(output_db.as_path())?;
        let modules = connection.query_row("SELECT COUNT(*) FROM modules", [], |row| {
            row.get::<_, i64>(0)
        })?;
        let source_files =
            connection.query_row("SELECT COUNT(*) FROM source_files", [], |row| {
                row.get::<_, i64>(0)
            })?;
        assert_eq!(source_files, 1);
        assert_eq!(modules, 0);
        let bundle =
            reverts_input::sqlite::load_project_bundle_from_sqlite(output_db.as_path(), 1)?;
        assert_eq!(bundle.source_files.len(), 1);
        assert!(bundle.modules.is_empty());
        Ok(())
    }
}
