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
use sha2::{Digest, Sha256};

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
    logical_path: String,
    physical_path: PathBuf,
    size: u64,
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
    size: u64,
    stored_path: PathBuf,
    package: Option<PackageOwner>,
    bundle_source: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImportEvidence {
    source_root: PathBuf,
    sources: Vec<EvidenceFile>,
    assets: Vec<EvidenceFile>,
    native_assets: Vec<EvidenceFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EvidenceFile {
    path: String,
    logical_path: Option<String>,
    size: Option<u64>,
    sha256: Option<String>,
    executable: bool,
    package: Option<PackageOwner>,
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
    let evidence = read_manifest(args.manifest.as_path())?;
    validate_evidence_source_root(&evidence, input_root.as_path(), args.manifest.as_path())?;
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
        &evidence,
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

    let private_package_roots = private_package_roots(&files);
    let private_package_source_assets = sources
        .iter()
        .filter(|source| is_private_package_source_asset(source, &private_package_roots))
        .count();
    let assets = files
        .iter()
        .filter(|file| file.kind == ImportFileKind::Asset)
        .count()
        + private_package_source_assets;
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

fn read_manifest(path: &Path) -> Result<ImportEvidence, ImportUnpackedError> {
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
    if value.get("schema").and_then(Value::as_str) != Some("reverts.import_evidence.v1") {
        return Err(ImportUnpackedError::UnsupportedManifest {
            path: path.to_path_buf(),
        });
    }
    let source_root = required_string(&value, "source_root", path)?;
    Ok(ImportEvidence {
        source_root: PathBuf::from(source_root),
        sources: parse_evidence_files(&value, "sources", path)?,
        assets: parse_evidence_files(&value, "assets", path)?,
        native_assets: parse_evidence_files(&value, "native_assets", path)?,
    })
}

fn required_string(
    value: &Value,
    field: &'static str,
    manifest: &Path,
) -> Result<String, ImportUnpackedError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| ImportUnpackedError::InvalidManifest {
            path: manifest.to_path_buf(),
            message: format!("missing string field {field}"),
        })
}

fn parse_evidence_files(
    value: &Value,
    field: &'static str,
    manifest: &Path,
) -> Result<Vec<EvidenceFile>, ImportUnpackedError> {
    let entries = value.get(field).and_then(Value::as_array).ok_or_else(|| {
        ImportUnpackedError::InvalidManifest {
            path: manifest.to_path_buf(),
            message: format!("missing array field {field}"),
        }
    })?;
    entries
        .iter()
        .map(|entry| parse_evidence_file(entry, field, manifest))
        .collect()
}

fn parse_evidence_file(
    value: &Value,
    field: &'static str,
    manifest: &Path,
) -> Result<EvidenceFile, ImportUnpackedError> {
    let Some(object) = value.as_object() else {
        return Err(ImportUnpackedError::InvalidManifest {
            path: manifest.to_path_buf(),
            message: format!("{field} entry is not an object"),
        });
    };
    let path = object
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| ImportUnpackedError::InvalidManifest {
            path: manifest.to_path_buf(),
            message: format!("{field} entry missing string path"),
        })?
        .to_string();
    let size = object.get("size").and_then(Value::as_u64);
    let logical_path = object
        .get("logical_path")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let sha256 = object
        .get("sha256")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let executable = object
        .get("executable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let package = object
        .get("package")
        .and_then(parse_package_owner_value)
        .transpose()
        .map_err(|message| ImportUnpackedError::InvalidManifest {
            path: manifest.to_path_buf(),
            message,
        })?;
    Ok(EvidenceFile {
        path,
        logical_path,
        size,
        sha256,
        executable,
        package,
    })
}

fn parse_package_owner_value(value: &Value) -> Option<Result<PackageOwner, String>> {
    if value.is_null() {
        return None;
    }
    let object = match value.as_object() {
        Some(object) => object,
        None => return Some(Err("package field is not an object".to_string())),
    };
    let package_name = match object.get("package_name").and_then(Value::as_str) {
        Some(value) if !value.is_empty() => value.to_string(),
        _ => return Some(Err("package field missing package_name".to_string())),
    };
    let package_root = match object.get("package_root").and_then(Value::as_str) {
        Some(value) if !value.is_empty() => value.to_string(),
        _ => return Some(Err("package field missing package_root".to_string())),
    };
    let package_version = object
        .get("package_version")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    Some(Ok(PackageOwner {
        package_name,
        package_version,
        package_root,
    }))
}

fn validate_evidence_source_root(
    evidence: &ImportEvidence,
    input_root: &Path,
    manifest: &Path,
) -> Result<(), ImportUnpackedError> {
    let source_root = evidence.source_root.canonicalize().map_err(|source| {
        ImportUnpackedError::ReadManifestSourceRoot {
            path: evidence.source_root.clone(),
            source,
        }
    })?;
    if source_root == input_root {
        Ok(())
    } else {
        Err(ImportUnpackedError::EvidenceInputMismatch {
            manifest: manifest.to_path_buf(),
            manifest_source_root: source_root,
            input_root: input_root.to_path_buf(),
        })
    }
}

fn collect_import_files(
    input_root: &Path,
    evidence: &ImportEvidence,
    ignore_native_assets: bool,
    max_source_bytes: Option<u64>,
    bundle_source_bytes: Option<u64>,
) -> Result<Vec<ImportFile>, ImportUnpackedError> {
    let disk_paths = collect_relative_disk_paths(input_root)?;
    let mut evidence_paths = BTreeSet::new();

    let mut files = Vec::new();
    for (evidence_file, mut kind) in evidence
        .sources
        .iter()
        .map(|file| (file, ImportFileKind::Source))
        .chain(
            evidence
                .assets
                .iter()
                .map(|file| (file, ImportFileKind::Asset)),
        )
        .chain(
            evidence
                .native_assets
                .iter()
                .map(|file| (file, ImportFileKind::NativeAsset)),
        )
    {
        validate_evidence_relative_path(evidence_file.path.as_str())?;
        if !evidence_paths.insert(evidence_file.path.clone()) {
            return Err(ImportUnpackedError::DuplicateManifestPath {
                path: evidence_file.path.clone(),
            });
        }
        let physical_path = input_root.join(Path::new(evidence_file.path.as_str()));
        let metadata = fs::metadata(physical_path.as_path()).map_err(|source| {
            ImportUnpackedError::EvidencePathMissing {
                path: physical_path.clone(),
                source,
            }
        })?;
        if !metadata.is_file() {
            return Err(ImportUnpackedError::EvidencePathNotFile {
                path: physical_path,
            });
        }
        let size = metadata.len();
        if let Some(expected_size) = evidence_file.size
            && size != expected_size
        {
            return Err(ImportUnpackedError::EvidenceSizeMismatch {
                path: evidence_file.path.clone(),
                expected: expected_size,
                actual: size,
            });
        }
        if let Some(expected_sha256) = &evidence_file.sha256 {
            let actual_sha256 = sha256_path(physical_path.as_path())?;
            if !expected_sha256.eq_ignore_ascii_case(actual_sha256.as_str()) {
                return Err(ImportUnpackedError::EvidenceHashMismatch {
                    path: evidence_file.path.clone(),
                    expected: expected_sha256.clone(),
                    actual: actual_sha256,
                });
            }
        }
        if ignore_native_assets && kind == ImportFileKind::NativeAsset {
            continue;
        }
        let package = evidence_file.package.clone();
        let logical_path = evidence_file
            .logical_path
            .clone()
            .unwrap_or_else(|| evidence_file.path.clone());
        let bundle_source = package.is_none()
            && kind == ImportFileKind::Source
            && bundle_source_bytes.is_some_and(|limit| size > limit);
        let deferred_source = !bundle_source
            && kind == ImportFileKind::Source
            && max_source_bytes.is_some_and(|limit| size > limit);
        if deferred_source {
            kind = ImportFileKind::Asset;
        }
        let executable = evidence_file.executable || is_executable(physical_path.as_path());
        files.push(ImportFile {
            relative_path: evidence_file.path.clone(),
            logical_path,
            physical_path,
            size,
            kind,
            package,
            executable,
            deferred_source,
            bundle_source,
        });
    }
    if let Some(missing) = disk_paths.difference(&evidence_paths).next() {
        return Err(ImportUnpackedError::ManifestMissingDiskFile {
            path: missing.clone(),
        });
    }
    Ok(files)
}

fn validate_evidence_relative_path(path: &str) -> Result<(), ImportUnpackedError> {
    let relative = Path::new(path);
    if path.is_empty() || relative.is_absolute() {
        return Err(ImportUnpackedError::InvalidEvidencePath {
            path: path.to_string(),
        });
    }
    for component in relative.components() {
        match component {
            Component::Normal(_) => {}
            _ => {
                return Err(ImportUnpackedError::InvalidEvidencePath {
                    path: path.to_string(),
                });
            }
        }
    }
    Ok(())
}

fn collect_relative_disk_paths(input_root: &Path) -> Result<BTreeSet<String>, ImportUnpackedError> {
    let mut paths = Vec::new();
    collect_file_paths(input_root, &mut paths)?;
    paths
        .iter()
        .map(|path| relative_path(input_root, path.as_path()))
        .collect()
}

fn sha256_path(path: &Path) -> Result<String, ImportUnpackedError> {
    let bytes = fs::read(path).map_err(|source| ImportUnpackedError::ReadEvidenceFile {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
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
            size: file.size,
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

/// Copy an asset's bytes next to the DB and return that path. Like
/// [`materialize_parseable_source`], this keeps `project_assets.source_path` —
/// read on every later generate — out of the volatile `--input` tree (e.g. an
/// app extracted under /tmp that macOS later cleans), so the project stays
/// self-contained and relocatable with its SQLite file.
fn materialize_asset(
    physical_path: &Path,
    relative_path: &str,
    args: &ImportUnpackedArgs,
) -> Result<PathBuf, ImportUnpackedError> {
    let parent = args
        .output_db
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stored_path = parent
        .join(".reverts-import-sources")
        .join(sanitize_path_segment(args.project_name.as_str()))
        .join(Path::new(relative_path));
    if let Some(dir) = stored_path.parent() {
        fs::create_dir_all(dir).map_err(|source| ImportUnpackedError::WriteSource {
            path: dir.to_path_buf(),
            source,
        })?;
    }
    fs::copy(physical_path, stored_path.as_path()).map_err(|source| {
        ImportUnpackedError::WriteSource {
            path: stored_path.clone(),
            source,
        }
    })?;
    Ok(stored_path)
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
    // Always materialize the source NEXT TO THE DB rather than referencing the
    // `--input` tree. The recorded `file_path` is read on every later
    // generate/match (the DB stores only paths, not bytes), so if it points into
    // a volatile location — e.g. an app extracted under /tmp, which macOS's
    // periodic tmp cleanup erodes — the project silently breaks days later. A
    // DB-adjacent copy keeps the project self-contained and relocatable with its
    // SQLite file. Shebang sources are additionally rewritten to a comment so the
    // parser accepts them.
    let parent = args
        .output_db
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stored_path = parent
        .join(".reverts-import-sources")
        .join(sanitize_path_segment(args.project_name.as_str()))
        .join(Path::new(file.relative_path.as_str()));
    if let Some(parent) = stored_path.parent() {
        fs::create_dir_all(parent).map_err(|source| ImportUnpackedError::WriteSource {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let stored_content = if content.starts_with("#!") {
        format!("//{}", content.strip_prefix("#").unwrap_or(content.as_str()))
    } else {
        content
    };
    fs::write(stored_path.as_path(), stored_content).map_err(|source| {
        ImportUnpackedError::WriteSource {
            path: stored_path.clone(),
            source,
        }
    })?;
    Ok(stored_path)
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

fn private_package_roots(files: &[ImportFile]) -> BTreeSet<String> {
    files
        .iter()
        .filter(|file| file.kind != ImportFileKind::Source)
        .filter(|file| file.relative_path.ends_with("/package.json"))
        .filter_map(|file| {
            let package = file.package.as_ref()?;
            let manifest = fs::read(file.physical_path.as_path()).ok()?;
            let value = serde_json::from_slice::<Value>(manifest.as_slice()).ok()?;
            if value.get("private").and_then(Value::as_bool) == Some(true)
                && value.get("name").and_then(Value::as_str) == Some(package.package_name.as_str())
            {
                Some(package.package_root.clone())
            } else {
                None
            }
        })
        .collect()
}

fn is_private_package_source_asset(
    source: &SourceRecord,
    private_package_roots: &BTreeSet<String>,
) -> bool {
    source
        .package
        .as_ref()
        .is_some_and(|package| private_package_roots.contains(package.package_root.as_str()))
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
        CREATE TABLE source_files (
            id INTEGER PRIMARY KEY,
            file_path TEXT NOT NULL,
            file_size INTEGER NOT NULL
        );
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
                "INSERT INTO source_files (id, file_path, file_size) VALUES (?1, ?2, ?3)",
                params![
                    source.id,
                    source.stored_path.to_string_lossy(),
                    i64::try_from(source.size).map_err(|_source| {
                        ImportUnpackedError::FileTooLarge {
                            path: source.relative_path.clone(),
                            size: source.size,
                        }
                    })?
                ],
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

    let private_package_roots = private_package_roots(files);
    let mut asset_id = 1_u32;
    for file in files
        .iter()
        .filter(|file| file.kind != ImportFileKind::Source)
    {
        let stored_path = materialize_asset(&file.physical_path, &file.relative_path, args)?;
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
                    file.logical_path,
                    format!("assets/{}", file.relative_path),
                    stored_path.to_string_lossy(),
                    asset_kind(file),
                    if file.executable { 1_i64 } else { 0_i64 }
                ],
            )
            .map_err(ImportUnpackedError::WriteDatabase)?;
        asset_id = asset_id
            .checked_add(1)
            .ok_or(ImportUnpackedError::TooManyFiles { count: usize::MAX })?;
    }

    for source in sources
        .iter()
        .filter(|source| is_private_package_source_asset(source, &private_package_roots))
    {
        let stored_path = materialize_asset(&source.physical_path, &source.relative_path, args)?;
        transaction
            .execute(
                r"
                INSERT INTO project_assets
                    (id, project_id, logical_path, output_path, source_path, kind, executable,
                     platform, arch)
                VALUES (?1, 1, ?2, ?3, ?4, 'data', 0, NULL, NULL)
                ",
                params![
                    asset_id,
                    source.relative_path,
                    format!("assets/{}", source.relative_path),
                    stored_path.to_string_lossy()
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
    ReadManifestSourceRoot {
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
    InvalidManifest {
        path: PathBuf,
        message: String,
    },
    EvidenceInputMismatch {
        manifest: PathBuf,
        manifest_source_root: PathBuf,
        input_root: PathBuf,
    },
    InvalidEvidencePath {
        path: String,
    },
    DuplicateManifestPath {
        path: String,
    },
    ManifestMissingDiskFile {
        path: String,
    },
    EvidencePathMissing {
        path: PathBuf,
        source: io::Error,
    },
    EvidencePathNotFile {
        path: PathBuf,
    },
    EvidenceSizeMismatch {
        path: String,
        expected: u64,
        actual: u64,
    },
    EvidenceHashMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    ReadEvidenceFile {
        path: PathBuf,
        source: io::Error,
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
    FileTooLarge {
        path: String,
        size: u64,
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
            Self::ReadManifestSourceRoot { path, source } => {
                write!(
                    formatter,
                    "failed to read manifest source_root {}: {source}",
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
                    "unsupported unpack manifest {}; expected reverts.import_evidence.v1",
                    path.display()
                )
            }
            Self::InvalidManifest { path, message } => {
                write!(formatter, "invalid manifest {}: {message}", path.display())
            }
            Self::EvidenceInputMismatch {
                manifest,
                manifest_source_root,
                input_root,
            } => {
                write!(
                    formatter,
                    "manifest {} source_root {} does not match --input {}",
                    manifest.display(),
                    manifest_source_root.display(),
                    input_root.display()
                )
            }
            Self::InvalidEvidencePath { path } => {
                write!(formatter, "invalid evidence path {path}")
            }
            Self::DuplicateManifestPath { path } => {
                write!(formatter, "duplicate path in import evidence: {path}")
            }
            Self::ManifestMissingDiskFile { path } => {
                write!(formatter, "manifest does not cover input file: {path}")
            }
            Self::EvidencePathMissing { path, source } => {
                write!(
                    formatter,
                    "manifest evidence file {} is missing: {source}",
                    path.display()
                )
            }
            Self::EvidencePathNotFile { path } => {
                write!(
                    formatter,
                    "manifest evidence path is not a file: {}",
                    path.display()
                )
            }
            Self::EvidenceSizeMismatch {
                path,
                expected,
                actual,
            } => {
                write!(
                    formatter,
                    "manifest evidence size mismatch for {path}: expected {expected}, got {actual}"
                )
            }
            Self::EvidenceHashMismatch {
                path,
                expected,
                actual,
            } => {
                write!(
                    formatter,
                    "manifest evidence sha256 mismatch for {path}: expected {expected}, got {actual}"
                )
            }
            Self::ReadEvidenceFile { path, source } => {
                write!(
                    formatter,
                    "failed to read evidence file {}: {source}",
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
            Self::FileTooLarge { path, size } => {
                write!(
                    formatter,
                    "source file {path} is too large for SQLite file_size: {size}"
                )
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
            | Self::ReadManifestSourceRoot { source, .. }
            | Self::EvidencePathMissing { source, .. }
            | Self::ReadEvidenceFile { source, .. }
            | Self::CreateOutputParent { source, .. }
            | Self::ReadDirectory { source, .. }
            | Self::ReadMetadata { source, .. }
            | Self::ReadSource { source, .. }
            | Self::WriteSource { source, .. } => Some(source),
            Self::ParseManifest { source, .. } => Some(source),
            Self::OpenOutputDatabase { source, .. }
            | Self::ConfigureDatabase(source)
            | Self::WriteDatabase(source) => Some(source),
            Self::InputRootNotDirectory(_)
            | Self::UnsupportedManifest { .. }
            | Self::InvalidManifest { .. }
            | Self::EvidenceInputMismatch { .. }
            | Self::InvalidEvidencePath { .. }
            | Self::DuplicateManifestPath { .. }
            | Self::ManifestMissingDiskFile { .. }
            | Self::EvidencePathNotFile { .. }
            | Self::EvidenceSizeMismatch { .. }
            | Self::EvidenceHashMismatch { .. }
            | Self::OutputDatabaseExists { .. }
            | Self::InvalidPath { .. }
            | Self::NoSourceFiles { .. }
            | Self::TooManyFiles { .. }
            | Self::FileTooLarge { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::path::Path;

    use rusqlite::Connection;
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    fn evidence_file(root: &Path, relative_path: &str, package: Option<Value>) -> Value {
        let path = root.join(relative_path);
        json!({
            "path": relative_path,
            "physical_path": path.to_string_lossy(),
            "size": fs::metadata(path.as_path()).expect("fixture metadata").len(),
            "sha256": sha256_path(path.as_path()).expect("fixture hash"),
            "executable": is_executable(path.as_path()),
            "package": package,
        })
    }

    fn evidence_file_with_logical_path(
        root: &Path,
        relative_path: &str,
        logical_path: &str,
        package: Option<Value>,
    ) -> Value {
        let mut value = evidence_file(root, relative_path, package);
        value["logical_path"] = json!(logical_path);
        value
    }

    fn package(package_name: &str, package_version: &str, package_root: &str) -> Value {
        json!({
            "package_name": package_name,
            "package_version": package_version,
            "package_root": package_root,
        })
    }

    fn write_evidence(
        root: &Path,
        manifest: &Path,
        sources: Vec<Value>,
        assets: Vec<Value>,
        native_assets: Vec<Value>,
    ) -> Result<(), Box<dyn Error>> {
        fs::write(
            manifest,
            serde_json::to_string_pretty(&json!({
                "schema": "reverts.import_evidence.v1",
                "target_kind": "electron_app",
                "source_root": root.to_string_lossy(),
                "entrypoints": [{"role": "electron_main", "path": "a.js"}],
                "sources": sources,
                "assets": assets,
                "native_assets": native_assets,
                "packages": [],
            }))?,
        )?;
        Ok(())
    }

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
        let ws_package = package("ws", "8.0.0", "node_modules/ws");
        write_evidence(
            root.as_path(),
            manifest.as_path(),
            vec![
                evidence_file(root.as_path(), "a.js", None),
                evidence_file(root.as_path(), "b.js", None),
                evidence_file(
                    root.as_path(),
                    "node_modules/ws/index.js",
                    Some(ws_package.clone()),
                ),
            ],
            vec![
                evidence_file(
                    root.as_path(),
                    "node_modules/ws/package.json",
                    Some(ws_package),
                ),
                evidence_file(root.as_path(), "style.css", None),
            ],
            vec![evidence_file(root.as_path(), "native.node", None)],
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
        let source_size = connection.query_row(
            "SELECT file_size FROM source_files WHERE id = 1",
            [],
            |row| row.get::<_, i64>(0),
        )?;
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
        assert_eq!(source_size, 26);
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
        let selections = crate::commands::runtime_inventory::runtime_inventory_project_selections(
            &crate::args::RuntimeInventoryArgs {
                project_id: Some(1),
                all_projects: false,
                limit: None,
                newest: false,
                max_source_bytes: None,
                setter_blockers: false,
                runtime_attribution: false,
                package_source_blockers: false,
                finding_clusters: false,
                init_cycles: false,
                input: output_db,
            },
        )?;
        assert_eq!(selections[0].source_bytes, 71);
        Ok(())
    }

    #[test]
    fn import_is_self_contained_after_input_tree_is_deleted() -> Result<(), Box<dyn Error>> {
        // Regression: source/asset bytes are read from disk on every later
        // generate/match (the DB stores only paths). They must be materialized
        // next to the DB, NOT referenced in the volatile `--input` tree — an app
        // extracted under /tmp is eroded by macOS's periodic tmp cleanup, silently
        // breaking the project days later. Deleting `--input` after import must
        // leave the project fully loadable.
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
        fs::write(root.join("native.node"), b"\xcf\xfa\xed\xfe")?;
        let manifest = temp.path().join("reverts-import-evidence.json");
        let ws_package = package("ws", "8.0.0", "node_modules/ws");
        write_evidence(
            root.as_path(),
            manifest.as_path(),
            vec![
                evidence_file(root.as_path(), "a.js", None),
                evidence_file(root.as_path(), "b.js", None),
                evidence_file(
                    root.as_path(),
                    "node_modules/ws/index.js",
                    Some(ws_package.clone()),
                ),
            ],
            vec![evidence_file(
                root.as_path(),
                "node_modules/ws/package.json",
                Some(ws_package),
            )],
            vec![evidence_file(root.as_path(), "native.node", None)],
        )?;
        let output_db = temp.path().join("project.sqlite");
        let args = ImportUnpackedArgs {
            input: root.clone(),
            manifest,
            project_name: "fixture".to_string(),
            output_db: output_db.clone(),
            ignore_native_assets: true,
            max_source_bytes: None,
            bundle_source_bytes: None,
        };

        import_unpacked_to_sqlite(&args)?;

        // The fragility trigger: the original input tree disappears.
        fs::remove_dir_all(root.as_path())?;

        // No recorded source/asset path may point back into the deleted input.
        let connection = Connection::open(output_db.as_path())?;
        let mut statement = connection.prepare("SELECT file_path FROM source_files")?;
        let source_paths = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        assert!(!source_paths.is_empty());
        for path in &source_paths {
            assert!(
                !path.starts_with(root.to_string_lossy().as_ref()),
                "source path still references the deleted --input tree: {path}"
            );
            assert!(
                std::path::Path::new(path).exists(),
                "materialized source must survive --input deletion: {path}"
            );
        }

        // The whole project still loads (reads every source/asset off disk).
        let bundle =
            reverts_input::sqlite::load_project_bundle_from_sqlite(output_db.as_path(), 1)?;
        assert_eq!(bundle.source_files.len(), 3);
        assert_eq!(bundle.assets.len(), 1);
        Ok(())
    }

    #[test]
    fn import_unpacked_preserves_manifest_asset_logical_path() -> Result<(), Box<dyn Error>> {
        let temp = tempdir()?;
        let root = temp.path().join("app");
        fs::create_dir_all(root.as_path())?;
        fs::write(
            root.join("addon.js"),
            "module.exports = require('/$bunfs/root/native.node');\n",
        )?;
        fs::write(root.join("native.node"), b"\x7fELFnative")?;
        let manifest = temp.path().join("reverts-import-evidence.json");
        write_evidence(
            root.as_path(),
            manifest.as_path(),
            vec![evidence_file(root.as_path(), "addon.js", None)],
            Vec::new(),
            vec![evidence_file_with_logical_path(
                root.as_path(),
                "native.node",
                "/$bunfs/root/native.node",
                None,
            )],
        )?;
        let output_db = temp.path().join("project.sqlite");
        let args = ImportUnpackedArgs {
            input: root,
            manifest,
            project_name: "fixture".to_string(),
            output_db: output_db.clone(),
            ignore_native_assets: false,
            max_source_bytes: None,
            bundle_source_bytes: None,
        };

        let outcome = import_unpacked_to_sqlite(&args)?;

        assert_eq!(outcome.native_assets, 1);
        let connection = Connection::open(output_db.as_path())?;
        let logical_path = connection.query_row(
            "SELECT logical_path FROM project_assets WHERE output_path = 'assets/native.node'",
            [],
            |row| row.get::<_, String>(0),
        )?;
        assert_eq!(logical_path, "/$bunfs/root/native.node");
        let bundle =
            reverts_input::sqlite::load_project_bundle_from_sqlite(output_db.as_path(), 1)?;
        assert_eq!(bundle.assets[0].logical_path, "/$bunfs/root/native.node");
        Ok(())
    }

    #[test]
    fn import_unpacked_materializes_private_package_sources_as_assets() -> Result<(), Box<dyn Error>>
    {
        let temp = tempdir()?;
        let root = temp.path().join("app");
        fs::create_dir_all(root.join("node_modules/@ant/private-native"))?;
        fs::write(root.join("main.js"), "require('@ant/private-native');\n")?;
        fs::write(
            root.join("node_modules/@ant/private-native/package.json"),
            r#"{"name":"@ant/private-native","version":"0.0.0","private":true,"main":"index.js"}"#,
        )?;
        fs::write(
            root.join("node_modules/@ant/private-native/index.js"),
            "module.exports = require('./binding.node');\n",
        )?;
        fs::write(
            root.join("node_modules/@ant/private-native/binding.node"),
            b"native",
        )?;
        let package = package(
            "@ant/private-native",
            "0.0.0",
            "node_modules/@ant/private-native",
        );
        let manifest = temp.path().join("reverts-import-evidence.json");
        write_evidence(
            root.as_path(),
            manifest.as_path(),
            vec![
                evidence_file(root.as_path(), "main.js", None),
                evidence_file(
                    root.as_path(),
                    "node_modules/@ant/private-native/index.js",
                    Some(package.clone()),
                ),
            ],
            vec![evidence_file(
                root.as_path(),
                "node_modules/@ant/private-native/package.json",
                Some(package.clone()),
            )],
            vec![evidence_file(
                root.as_path(),
                "node_modules/@ant/private-native/binding.node",
                Some(package),
            )],
        )?;
        let output_db = temp.path().join("project.sqlite");
        let args = ImportUnpackedArgs {
            input: root,
            manifest,
            project_name: "fixture".to_string(),
            output_db: output_db.clone(),
            ignore_native_assets: false,
            max_source_bytes: None,
            bundle_source_bytes: None,
        };

        let outcome = import_unpacked_to_sqlite(&args)?;

        assert_eq!(outcome.assets, 2);
        assert_eq!(outcome.native_assets, 1);
        let connection = Connection::open(output_db.as_path())?;
        let source_asset = connection.query_row(
            "SELECT kind FROM project_assets WHERE logical_path = ?1",
            ["node_modules/@ant/private-native/index.js"],
            |row| row.get::<_, String>(0),
        )?;
        assert_eq!(source_asset, "data");
        let bundle =
            reverts_input::sqlite::load_project_bundle_from_sqlite(output_db.as_path(), 1)?;
        assert!(bundle.assets.iter().any(|asset| {
            asset.output_path == "assets/node_modules/@ant/private-native/index.js"
                && asset.bytes == b"module.exports = require('./binding.node');\n"
        }));
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
        write_evidence(
            root.as_path(),
            manifest.as_path(),
            vec![
                evidence_file(root.as_path(), "small.js", None),
                evidence_file(root.as_path(), "large.js", None),
            ],
            Vec::new(),
            Vec::new(),
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
        write_evidence(
            root.as_path(),
            manifest.as_path(),
            vec![evidence_file(root.as_path(), "bundle.js", None)],
            Vec::new(),
            Vec::new(),
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

    #[test]
    fn import_unpacked_rejects_manifest_hash_mismatch() -> Result<(), Box<dyn Error>> {
        let temp = tempdir()?;
        let root = temp.path().join("app");
        fs::create_dir_all(root.as_path())?;
        fs::write(root.join("main.js"), "export const ok = true;\n")?;
        let manifest = temp.path().join("reverts-import-evidence.json");
        fs::write(
            manifest.as_path(),
            serde_json::to_string_pretty(&json!({
                "schema": "reverts.import_evidence.v1",
                "target_kind": "electron_app",
                "source_root": root.to_string_lossy(),
                "sources": [{
                    "path": "main.js",
                    "size": fs::metadata(root.join("main.js"))?.len(),
                    "sha256": "0000000000000000000000000000000000000000000000000000000000000000",
                    "executable": false,
                    "package": null,
                }],
                "assets": [],
                "native_assets": [],
            }))?,
        )?;
        let output_db = temp.path().join("project.sqlite");
        let args = ImportUnpackedArgs {
            input: root,
            manifest,
            project_name: "fixture".to_string(),
            output_db,
            ignore_native_assets: true,
            max_source_bytes: None,
            bundle_source_bytes: None,
        };

        let error = import_unpacked_to_sqlite(&args).expect_err("hash mismatch must fail");

        assert!(matches!(
            error,
            ImportUnpackedError::EvidenceHashMismatch { .. }
        ));
        Ok(())
    }
}
