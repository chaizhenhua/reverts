//! `extract-assets` command runner + the full asset-discovery /
//! materialization pipeline.
//!
//! The CLI dispatcher invokes [`run`], which calls
//! [`extract_assets_from_sqlite`] and prints a one-line summary. The rest
//! of the module owns: discovering on-disk and bun-embedded assets that
//! match the project's collected `AssetReference`s, materializing
//! embedded payloads, persisting rows into `project_assets`, and the
//! ELF / WASM size parsers used to slice embedded blobs out of
//! single-file Bun executables.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use reverts_input::sqlite::load_project_rows_from_connection;
use reverts_input::{AssetKind, InputRows, ModuleInput, SourceFileInput};
use reverts_pipeline::{AssetReference, collect_required_asset_references_from_rows};
use rusqlite::{Connection, OpenFlags, params};

use crate::args::ExtractAssetsArgs;
use crate::errors::{CliRunError, ExtractAssetsError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractAssetsOutcome {
    pub project_id: u32,
    pub referenced_assets: usize,
    pub matched_assets: usize,
    pub missing_assets: usize,
    pub written_assets: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiscoveredProjectAsset {
    pub(crate) reference: AssetReference,
    pub(crate) source: DiscoveredAssetSource,
    pub(crate) output_path: String,
    pub(crate) kind: AssetKind,
    pub(crate) executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiscoveredAssetSource {
    File(PathBuf),
    EmbeddedBunFile { bytes: Vec<u8> },
}

pub(crate) fn run(args: ExtractAssetsArgs) -> Result<(), CliRunError> {
    let outcome = extract_assets_from_sqlite(&args).map_err(CliRunError::ExtractAssets)?;
    println!(
        "extracted assets for project {}: {} reference(s), {} matched, {} missing, {} written",
        outcome.project_id,
        outcome.referenced_assets,
        outcome.matched_assets,
        outcome.missing_assets,
        outcome.written_assets
    );
    Ok(())
}

pub fn extract_assets_from_sqlite(
    args: &ExtractAssetsArgs,
) -> Result<ExtractAssetsOutcome, ExtractAssetsError> {
    let flags = if args.apply {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let mut connection =
        Connection::open_with_flags(args.input.as_path(), flags).map_err(|source| {
            ExtractAssetsError::OpenDatabase {
                path: args.input.clone(),
                source,
            }
        })?;
    connection
        .busy_timeout(Duration::from_secs(30))
        .map_err(ExtractAssetsError::ConfigureDatabase)?;
    connection
        .execute_batch("PRAGMA foreign_keys = ON")
        .map_err(ExtractAssetsError::ConfigureDatabase)?;
    extract_assets_from_connection(&mut connection, args)
}

pub fn extract_assets_from_connection(
    connection: &mut Connection,
    args: &ExtractAssetsArgs,
) -> Result<ExtractAssetsOutcome, ExtractAssetsError> {
    let rows = load_project_rows_from_connection(connection, args.project_id)
        .map_err(ExtractAssetsError::LoadInput)?;
    let references = collect_required_asset_references_from_rows(&rows);
    let referenced_logical_paths = references
        .iter()
        .map(|reference| reference.logical_path.as_str())
        .collect::<BTreeSet<_>>();
    let discovered = discover_project_assets(&rows, &references, &args.asset_roots)?;
    let written_assets = if args.apply {
        let materialized_root = materialized_asset_root(args.input.as_path(), rows.project.id);
        persist_project_assets(
            connection,
            rows.project.id,
            &discovered,
            materialized_root.as_path(),
        )?
    } else {
        0
    };

    Ok(ExtractAssetsOutcome {
        project_id: rows.project.id,
        referenced_assets: referenced_logical_paths.len(),
        matched_assets: discovered.len(),
        missing_assets: referenced_logical_paths
            .len()
            .saturating_sub(discovered.len()),
        written_assets,
    })
}

fn discover_project_assets(
    rows: &InputRows,
    references: &[AssetReference],
    asset_roots: &[PathBuf],
) -> Result<Vec<DiscoveredProjectAsset>, ExtractAssetsError> {
    let default_asset_root =
        common_source_root(&rows.source_files).ok_or(ExtractAssetsError::CannotInferAssetRoot {
            project_id: rows.project.id,
        })?;
    let effective_asset_roots = if asset_roots.is_empty() {
        vec![default_asset_root]
    } else {
        asset_roots.to_vec()
    };
    let modules = rows
        .modules
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let source_files = rows
        .source_files
        .iter()
        .map(|source_file| (source_file.id, source_file))
        .collect::<BTreeMap<_, _>>();
    let mut discovered = Vec::new();
    let mut seen_logical_paths = BTreeSet::new();

    for reference in references {
        if !seen_logical_paths.insert(reference.logical_path.as_str()) {
            continue;
        }
        let Some(module) = modules.get(&reference.module_id).copied() else {
            continue;
        };
        let Some(source_file) = module
            .source_file_id
            .and_then(|source_file_id| source_files.get(&source_file_id).copied())
        else {
            continue;
        };
        let source = discover_asset_source_from_roots(
            reference.logical_path.as_str(),
            source_file.path.as_str(),
            &effective_asset_roots,
        )?;
        let Some(source) = source else {
            continue;
        };
        let Some(output_path) = asset_output_path(module, reference.logical_path.as_str()) else {
            continue;
        };
        discovered.push(DiscoveredProjectAsset {
            reference: reference.clone(),
            source,
            output_path,
            kind: infer_asset_kind(reference.logical_path.as_str()),
            executable: infer_asset_executable(reference.logical_path.as_str()),
        });
    }

    Ok(discovered)
}

fn discover_asset_source_from_roots(
    logical_path: &str,
    source_file_path: &str,
    asset_roots: &[PathBuf],
) -> Result<Option<DiscoveredAssetSource>, ExtractAssetsError> {
    let mut matches = Vec::new();
    for asset_root in asset_roots {
        if let Some(source) =
            discover_asset_source(logical_path, source_file_path, asset_root.as_path())?
        {
            matches.push(source);
        }
    }

    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        _ => Err(ExtractAssetsError::AmbiguousAsset {
            logical_path: logical_path.to_string(),
            candidates: matches
                .iter()
                .map(DiscoveredAssetSource::description)
                .collect(),
        }),
    }
}

fn discover_asset_source(
    logical_path: &str,
    source_file_path: &str,
    asset_root: &Path,
) -> Result<Option<DiscoveredAssetSource>, ExtractAssetsError> {
    if bun_root_relative_path(logical_path).is_some()
        && asset_root.is_file()
        && let Some(bytes) = extract_bun_embedded_asset(asset_root, logical_path)?
    {
        return Ok(Some(DiscoveredAssetSource::EmbeddedBunFile { bytes }));
    }

    let physical_asset_root = if asset_root.is_file() {
        asset_root.parent().unwrap_or_else(|| Path::new(""))
    } else {
        asset_root
    };
    let source_path = asset_source_path(logical_path, source_file_path, physical_asset_root);
    if source_path.is_file() {
        Ok(Some(DiscoveredAssetSource::File(source_path)))
    } else {
        Ok(None)
    }
}

impl DiscoveredAssetSource {
    fn description(&self) -> String {
        match self {
            Self::File(path) => path.to_string_lossy().into_owned(),
            Self::EmbeddedBunFile { bytes } => {
                format!("embedded bun payload ({} bytes)", bytes.len())
            }
        }
    }
}

fn asset_source_path(logical_path: &str, source_file_path: &str, asset_root: &Path) -> PathBuf {
    if let Some(root_relative) = bun_root_relative_path(logical_path) {
        return asset_root.join(root_relative);
    }
    let logical = Path::new(logical_path);
    if logical_path.starts_with("./") || logical_path.starts_with("../") {
        return Path::new(source_file_path)
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(logical);
    }
    asset_root.join(logical)
}

fn asset_output_path(module: &ModuleInput, logical_path: &str) -> Option<String> {
    let module_dir = Path::new(module.semantic_path.as_str())
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let relative = output_relative_asset_path(logical_path)?;
    let mut output = module_dir.to_path_buf();
    output.push(relative);
    Some(path_to_forward_slashes(output.as_path()))
}

fn output_relative_asset_path(logical_path: &str) -> Option<PathBuf> {
    let logical = bun_root_relative_path(logical_path).unwrap_or(logical_path);
    let mut output = PathBuf::new();
    for component in Path::new(logical).components() {
        match component {
            Component::Normal(part) => output.push(part),
            Component::CurDir | Component::ParentDir => {}
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!output.as_os_str().is_empty()).then_some(output)
}

fn bun_root_relative_path(logical_path: &str) -> Option<&str> {
    logical_path
        .strip_prefix("/$bunfs/root/")
        .or_else(|| logical_path.strip_prefix("bun:/root/"))
}

fn common_source_root(source_files: &[SourceFileInput]) -> Option<PathBuf> {
    let mut parents = source_files
        .iter()
        .map(|source_file| Path::new(source_file.path.as_str()))
        .filter_map(Path::parent);
    let first = parents.next()?.to_path_buf();
    Some(parents.fold(first, common_path_prefix))
}

fn common_path_prefix(left: PathBuf, right: &Path) -> PathBuf {
    let left_components = left.components().collect::<Vec<_>>();
    let right_components = right.components().collect::<Vec<_>>();
    let mut output = PathBuf::new();
    for (left, right) in left_components.iter().zip(right_components.iter()) {
        if left != right {
            break;
        }
        output.push(left.as_os_str());
    }
    output
}

fn path_to_forward_slashes(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            Component::CurDir => None,
            Component::ParentDir => Some("..".to_string()),
            Component::RootDir | Component::Prefix(_) => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn infer_asset_kind(logical_path: &str) -> AssetKind {
    let extension = Path::new(logical_path)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("wasm") => AssetKind::Wasm,
        Some("node") => AssetKind::NativeNode,
        Some("exe") => AssetKind::Executable,
        Some("png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "avif" | "ico") => AssetKind::Image,
        Some("ttf" | "otf" | "woff" | "woff2") => AssetKind::Font,
        Some("css") => AssetKind::Css,
        Some("html" | "htm") => AssetKind::Html,
        _ if infer_asset_executable(logical_path) => AssetKind::Executable,
        _ => AssetKind::Data,
    }
}

fn infer_asset_executable(logical_path: &str) -> bool {
    Path::new(logical_path)
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .map(|name| matches!(name, "rg" | "rg.exe" | "ripgrep" | "ripgrep.exe"))
        .unwrap_or(false)
        || Path::new(logical_path)
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
}

fn extract_bun_embedded_asset(
    executable_path: &Path,
    logical_path: &str,
) -> Result<Option<Vec<u8>>, ExtractAssetsError> {
    let bytes = fs::read(executable_path).map_err(|source| ExtractAssetsError::ReadAsset {
        path: executable_path.to_path_buf(),
        source,
    })?;
    Ok(extract_bun_embedded_asset_from_bytes(
        bytes.as_slice(),
        logical_path,
    ))
}

pub(crate) fn extract_bun_embedded_asset_from_bytes(
    executable: &[u8],
    logical_path: &str,
) -> Option<Vec<u8>> {
    let needle = logical_path.as_bytes();
    if needle.is_empty() {
        return None;
    }
    let mut cursor = 0usize;
    while let Some(relative) = find_bytes(&executable[cursor..], needle) {
        let path_start = cursor + relative;
        let data_start = path_start.checked_add(needle.len())?.checked_add(1)?;
        if executable.get(path_start + needle.len()).copied() != Some(0) {
            cursor = path_start + 1;
            continue;
        }
        let payload = executable.get(data_start..)?;
        if let Some(size) = embedded_asset_payload_size(payload)
            && data_start.checked_add(size)? <= executable.len()
        {
            return Some(payload[..size].to_vec());
        }
        cursor = path_start + 1;
    }
    None
}

fn embedded_asset_payload_size(payload: &[u8]) -> Option<usize> {
    parse_elf_file_size(payload)
        .or_else(|| parse_wasm_file_size(payload))
        .filter(|size| *size > 0 && *size <= payload.len())
}

fn parse_elf_file_size(payload: &[u8]) -> Option<usize> {
    if payload.len() < 0x40 || &payload[..4] != b"\x7fELF" || payload.get(5).copied()? != 1 {
        return None;
    }
    match payload.get(4).copied()? {
        1 => parse_elf32_file_size(payload),
        2 => parse_elf64_file_size(payload),
        _ => None,
    }
}

fn parse_elf64_file_size(payload: &[u8]) -> Option<usize> {
    let phoff = read_u64(payload, 0x20)?;
    let shoff = read_u64(payload, 0x28)?;
    let ehsize = u64::from(read_u16(payload, 0x34)?);
    let phentsize = u64::from(read_u16(payload, 0x36)?);
    let phnum = u64::from(read_u16(payload, 0x38)?);
    let shentsize = u64::from(read_u16(payload, 0x3a)?);
    let shnum = u64::from(read_u16(payload, 0x3c)?);
    let mut size = ehsize;
    size = size.max(table_end(phoff, phentsize, phnum)?);
    size = size.max(table_end(shoff, shentsize, shnum)?);
    for index in 0..phnum {
        let header = phoff.checked_add(index.checked_mul(phentsize)?)?;
        let p_offset = read_u64(payload, usize::try_from(header.checked_add(0x08)?).ok()?)?;
        let p_filesz = read_u64(payload, usize::try_from(header.checked_add(0x20)?).ok()?)?;
        size = size.max(p_offset.checked_add(p_filesz)?);
    }
    usize::try_from(size).ok()
}

fn parse_elf32_file_size(payload: &[u8]) -> Option<usize> {
    if payload.len() < 0x34 {
        return None;
    }
    let phoff = u64::from(read_u32(payload, 0x1c)?);
    let shoff = u64::from(read_u32(payload, 0x20)?);
    let ehsize = u64::from(read_u16(payload, 0x28)?);
    let phentsize = u64::from(read_u16(payload, 0x2a)?);
    let phnum = u64::from(read_u16(payload, 0x2c)?);
    let shentsize = u64::from(read_u16(payload, 0x2e)?);
    let shnum = u64::from(read_u16(payload, 0x30)?);
    let mut size = ehsize;
    size = size.max(table_end(phoff, phentsize, phnum)?);
    size = size.max(table_end(shoff, shentsize, shnum)?);
    for index in 0..phnum {
        let header = phoff.checked_add(index.checked_mul(phentsize)?)?;
        let p_offset = u64::from(read_u32(
            payload,
            usize::try_from(header.checked_add(0x04)?).ok()?,
        )?);
        let p_filesz = u64::from(read_u32(
            payload,
            usize::try_from(header.checked_add(0x10)?).ok()?,
        )?);
        size = size.max(p_offset.checked_add(p_filesz)?);
    }
    usize::try_from(size).ok()
}

fn parse_wasm_file_size(payload: &[u8]) -> Option<usize> {
    if payload.len() < 8 || &payload[..4] != b"\0asm" {
        return None;
    }
    let mut cursor = 8usize;
    let mut last_non_custom_section = 0u8;
    while cursor < payload.len() {
        let section_start = cursor;
        let section_id = *payload.get(cursor)?;
        if section_id > 12 {
            return Some(section_start);
        }
        cursor = cursor.checked_add(1)?;
        let Some((section_len, next)) = read_leb128_usize(payload, cursor) else {
            return Some(section_start);
        };
        if section_id != 0 {
            if section_id <= last_non_custom_section {
                return Some(section_start);
            }
            last_non_custom_section = section_id;
        }
        let Some(next_cursor) = next.checked_add(section_len) else {
            return Some(section_start);
        };
        if next_cursor > payload.len() {
            return Some(section_start);
        }
        cursor = next_cursor;
    }
    Some(cursor)
}

fn read_leb128_usize(payload: &[u8], mut cursor: usize) -> Option<(usize, usize)> {
    let mut value = 0usize;
    let mut shift = 0usize;
    loop {
        let byte = *payload.get(cursor)?;
        cursor += 1;
        value |= usize::from(byte & 0x7f).checked_shl(u32::try_from(shift).ok()?)?;
        if byte & 0x80 == 0 {
            return Some((value, cursor));
        }
        shift = shift.checked_add(7)?;
        if shift >= usize::BITS as usize {
            return None;
        }
    }
}

fn table_end(offset: u64, entry_size: u64, count: u64) -> Option<u64> {
    if offset == 0 || entry_size == 0 || count == 0 {
        return Some(0);
    }
    offset.checked_add(entry_size.checked_mul(count)?)
}

fn read_u16(payload: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        payload
            .get(offset..offset.checked_add(2)?)?
            .try_into()
            .ok()?,
    ))
}

fn read_u32(payload: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        payload
            .get(offset..offset.checked_add(4)?)?
            .try_into()
            .ok()?,
    ))
}

fn read_u64(payload: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        payload
            .get(offset..offset.checked_add(8)?)?
            .try_into()
            .ok()?,
    ))
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn materialized_asset_root(database_path: &Path, project_id: u32) -> PathBuf {
    let database_dir = database_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    database_dir
        .join("project-assets")
        .join(project_id.to_string())
}

fn persist_project_assets(
    connection: &mut Connection,
    project_id: u32,
    assets: &[DiscoveredProjectAsset],
    materialized_root: &Path,
) -> Result<usize, ExtractAssetsError> {
    if assets.is_empty() {
        return Ok(0);
    }
    ensure_project_assets_table(connection)?;
    let transaction = connection
        .transaction()
        .map_err(ExtractAssetsError::WriteAsset)?;
    let mut written = 0;
    for asset in assets {
        persist_project_asset(&transaction, project_id, asset, materialized_root)?;
        written += 1;
    }
    transaction
        .commit()
        .map_err(ExtractAssetsError::WriteAsset)?;
    Ok(written)
}

fn ensure_project_assets_table(connection: &Connection) -> Result<(), ExtractAssetsError> {
    connection
        .execute_batch(
            r"
            CREATE TABLE IF NOT EXISTS project_assets (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_id INTEGER NOT NULL,
                logical_path TEXT NOT NULL,
                output_path TEXT NOT NULL,
                source_path TEXT NOT NULL,
                kind TEXT NOT NULL,
                executable INTEGER NOT NULL DEFAULT 0,
                platform TEXT,
                arch TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE (project_id, logical_path),
                UNIQUE (project_id, output_path)
            );
            ",
        )
        .map_err(ExtractAssetsError::WriteAsset)
}

fn persist_project_asset(
    connection: &Connection,
    project_id: u32,
    asset: &DiscoveredProjectAsset,
    materialized_root: &Path,
) -> Result<(), ExtractAssetsError> {
    let source_path = materialize_project_asset_source(asset, materialized_root)?;
    connection
        .execute(
            "DELETE FROM project_assets WHERE project_id = ?1 AND logical_path = ?2",
            params![i64::from(project_id), asset.reference.logical_path.as_str()],
        )
        .map_err(ExtractAssetsError::WriteAsset)?;
    connection
        .execute(
            r"
            INSERT INTO project_assets
                (project_id, logical_path, output_path, source_path, kind, executable,
                 platform, arch, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, datetime('now'), datetime('now'))
            ",
            params![
                i64::from(project_id),
                asset.reference.logical_path.as_str(),
                asset.output_path.as_str(),
                source_path.to_string_lossy().as_ref(),
                asset.kind.as_str(),
                if asset.executable { 1_i64 } else { 0_i64 },
            ],
        )
        .map_err(ExtractAssetsError::WriteAsset)?;
    Ok(())
}

fn materialize_project_asset_source(
    asset: &DiscoveredProjectAsset,
    materialized_root: &Path,
) -> Result<PathBuf, ExtractAssetsError> {
    match &asset.source {
        DiscoveredAssetSource::File(path) => Ok(path.clone()),
        DiscoveredAssetSource::EmbeddedBunFile { bytes, .. } => {
            let relative = output_relative_asset_path(asset.reference.logical_path.as_str())
                .ok_or_else(|| ExtractAssetsError::InvalidAssetPath {
                    logical_path: asset.reference.logical_path.clone(),
                })?;
            let path = materialized_root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|source| {
                    ExtractAssetsError::WriteMaterializedAsset {
                        path: parent.to_path_buf(),
                        source,
                    }
                })?;
            }
            fs::write(path.as_path(), bytes).map_err(|source| {
                ExtractAssetsError::WriteMaterializedAsset {
                    path: path.clone(),
                    source,
                }
            })?;
            set_materialized_executable_bit(path.as_path(), asset.executable).map_err(
                |source| ExtractAssetsError::WriteMaterializedAsset {
                    path: path.clone(),
                    source,
                },
            )?;
            Ok(path)
        }
    }
}

#[cfg(unix)]
fn set_materialized_executable_bit(path: &Path, executable: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if !executable {
        return Ok(());
    }
    let metadata = fs::metadata(path)?;
    let mut permissions = metadata.permissions();
    permissions.set_mode(permissions.mode() | 0o755);
    fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn set_materialized_executable_bit(_path: &Path, _executable: bool) -> io::Result<()> {
    Ok(())
}
