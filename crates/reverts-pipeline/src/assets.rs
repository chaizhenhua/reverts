//! Asset reference collection, audit, and emit-time rewriting.
//!
//! - `collect_required_asset_references` / `_from_rows` walk module
//!   source slices for `require(...)`/`import(...)` literals that resolve
//!   to a known asset path.
//!   * Static `bun:/$bunfs` artefact paths and platform-specific binaries
//!     are detected by `is_asset_reference_literal`.
//! - `audit_required_assets` flags references that the bundle expects
//!   but `project_assets` doesn't supply (per ADR 0002 this is a warning;
//!   the emitted source still references the missing asset).
//! - `collect_emitted_assets` carries every input asset forward. Some
//!   decompilation targets contain assets that are loaded through dynamic
//!   paths not recoverable from static source analysis; preserving the full
//!   `project_assets` evidence set keeps output complete while
//!   `audit_required_assets` still validates known references.
//! - `rewrite_emitted_asset_references` rewrites the literal in the
//!   generated source so each module imports its asset through the
//!   chosen `output_path`, relative to where the module lands.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use reverts_emitter::EmittedProject;
use reverts_input::{InputBundle, InputRows, ModuleInput, SourceFileInput};
use reverts_ir::{ModuleId, ModuleKind};
use reverts_js::{ParseGoal, collect_path_builder_calls, collect_static_resource_specifiers};
use reverts_observe::{AuditFinding, AuditReport, FindingCode};

use crate::{
    AssetReference, EmittedAsset, relative_asset_specifier, rewrite_string_literal_values,
};

pub(crate) fn collect_emitted_assets(
    input: &InputBundle,
    _references: &[AssetReference],
) -> Vec<EmittedAsset> {
    input
        .assets
        .iter()
        .map(|asset| EmittedAsset {
            path: asset.output_path.clone(),
            bytes: asset.bytes.clone(),
            executable: asset.executable,
        })
        .collect()
}

#[must_use]
pub fn collect_required_asset_references(input: &InputBundle) -> Vec<AssetReference> {
    collect_required_asset_references_from_parts(&input.modules, &input.source_files, |module_id| {
        input
            .module_source_slice(module_id)
            .map(|slice| (slice.source_file_path.to_string(), slice.source.to_string()))
    })
}

#[must_use]
pub fn collect_required_asset_references_from_rows(rows: &InputRows) -> Vec<AssetReference> {
    collect_required_asset_references_from_parts(&rows.modules, &rows.source_files, |module_id| {
        rows.module_source_slice(module_id)
            .map(|slice| (slice.source_file_path.to_string(), slice.source.to_string()))
    })
}

pub(crate) fn collect_required_asset_references_from_parts(
    modules: &[ModuleInput],
    _source_files: &[SourceFileInput],
    source_for_module: impl Fn(ModuleId) -> Option<(String, String)>,
) -> Vec<AssetReference> {
    let mut references = BTreeSet::new();
    for module in modules {
        if module.kind == ModuleKind::Package {
            continue;
        }
        let Some((source_file_path, source)) = source_for_module(module.id) else {
            continue;
        };
        let Ok(literals) = collect_static_resource_specifiers(
            source.as_str(),
            Some(Path::new(source_file_path.as_str())),
            ParseGoal::TypeScript,
        ) else {
            // Parse failures are already surfaced by AstFactExtractionFailed
            // during enrichment.
            continue;
        };
        for literal in literals {
            if is_asset_reference_literal(literal.value.as_str()) {
                references.insert(AssetReference {
                    module_id: module.id,
                    logical_path: literal.value,
                });
            }
        }
        for logical_path in
            collect_dynamic_asset_references(source.as_str(), source_file_path.as_str())
        {
            references.insert(AssetReference {
                module_id: module.id,
                logical_path,
            });
        }
    }
    references.into_iter().collect()
}

pub(crate) fn collect_dynamic_asset_references(
    source: &str,
    source_file_path: &str,
) -> Vec<String> {
    let Ok(path_calls) = collect_path_builder_calls(
        source,
        Some(Path::new(source_file_path)),
        ParseGoal::TypeScript,
    ) else {
        return Vec::new();
    };

    let values = path_calls
        .iter()
        .flat_map(|call| call.string_arguments.iter().map(String::as_str))
        .collect::<BTreeSet<_>>();
    let has_ripgrep_vendor_prefix = path_calls
        .iter()
        .any(|call| contains_adjacent_segments(&call.string_arguments, &["vendor", "ripgrep"]))
        || (values.contains("vendor") && values.contains("ripgrep"));

    if !has_ripgrep_vendor_prefix {
        return Vec::new();
    }

    let mut references = BTreeSet::<String>::new();
    for call in &path_calls {
        for platform_dir in call
            .string_arguments
            .iter()
            .map(String::as_str)
            .filter(|value| is_node_platform_dir(value))
        {
            if call.string_arguments.iter().any(|value| value == "rg") {
                references.insert(format!("vendor/ripgrep/{platform_dir}/rg"));
            }
            if call.string_arguments.iter().any(|value| value == "rg.exe") {
                references.insert(format!("vendor/ripgrep/{platform_dir}/rg.exe"));
            }
        }

        let call_source = source
            .get(call.byte_start as usize..call.byte_end as usize)
            .unwrap_or_default();
        if call.string_arguments.iter().any(|value| value == "rg")
            && call_source.contains("process.arch")
            && call_source.contains("process.platform")
            && let Some(platform_dir) = current_node_platform_dir()
        {
            references.insert(format!("vendor/ripgrep/{platform_dir}/rg"));
        }
    }

    references.into_iter().collect()
}

fn contains_adjacent_segments(arguments: &[String], segments: &[&str]) -> bool {
    if segments.is_empty() || arguments.len() < segments.len() {
        return false;
    }
    arguments.windows(segments.len()).any(|window| {
        window
            .iter()
            .map(String::as_str)
            .eq(segments.iter().copied())
    })
}

fn is_node_platform_dir(value: &str) -> bool {
    let Some((arch, platform)) = value.split_once('-') else {
        return false;
    };
    matches!(arch, "x64" | "arm64" | "arm") && matches!(platform, "linux" | "darwin" | "win32")
}

pub(crate) fn current_node_platform_dir() -> Option<String> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "arm" => "arm",
        _ => return None,
    };
    let platform = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        "windows" => "win32",
        _ => return None,
    };
    Some(format!("{arch}-{platform}"))
}

pub(crate) fn audit_required_assets(
    input: &InputBundle,
    references: &[AssetReference],
) -> AuditReport {
    let available = input
        .assets
        .iter()
        .map(|asset| asset.logical_path.as_str())
        .collect::<BTreeSet<_>>();
    let mut audit = AuditReport::default();
    for reference in references {
        if available.contains(reference.logical_path.as_str()) {
            continue;
        }
        // The input bundle references an asset whose binary is not in
        // project_assets. Per ADR 0002 the decompiler is faithful, not
        // corrective: emit the reference, surface the missing asset, and
        // let the operator backfill the asset table.
        audit.push(
            AuditFinding::warning(
                FindingCode::MissingRequiredAsset,
                "source references an asset that is absent from project_assets",
            )
            .with_module(reference.module_id.0.to_string())
            .with_binding(reference.logical_path.clone()),
        );
    }
    audit
}

fn is_asset_reference_literal(value: &str) -> bool {
    let path = strip_query_and_fragment(value);
    if path.starts_with("/$bunfs/root/") || path.starts_with("bun:/") {
        return true;
    }

    if path.trim() != path || path.is_empty() || path.chars().any(char::is_whitespace) {
        return false;
    }
    let is_relative = path.starts_with("./") || path.starts_with("../");
    let is_absolute = path.starts_with('/');
    let is_vendor_path = path.starts_with("vendor/") || path.contains("/vendor/");
    let lower = path.to_ascii_lowercase();
    let has_asset_extension = matches!(
        Path::new(lower.as_str())
            .extension()
            .and_then(std::ffi::OsStr::to_str),
        Some(
            "wasm"
                | "node"
                | "so"
                | "dylib"
                | "dll"
                | "exe"
                | "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "svg"
                | "webp"
                | "avif"
                | "ico"
                | "ttf"
                | "otf"
                | "woff"
                | "woff2"
                | "css"
                | "html"
        )
    );

    has_asset_extension && (is_relative || is_absolute || is_vendor_path)
}

fn strip_query_and_fragment(value: &str) -> &str {
    let query_index = value.find('?').unwrap_or(value.len());
    let fragment_index = value.find('#').unwrap_or(value.len());
    &value[..query_index.min(fragment_index)]
}

pub(crate) fn rewrite_emitted_asset_references(
    project: &mut EmittedProject,
    input: &InputBundle,
    references: &[AssetReference],
    module_output_paths: &BTreeMap<ModuleId, String>,
) {
    let assets_by_logical_path = input
        .assets
        .iter()
        .map(|asset| (asset.logical_path.as_str(), asset.output_path.as_str()))
        .collect::<BTreeMap<_, _>>();
    let mut rewrites_by_file = BTreeMap::<String, BTreeMap<String, String>>::new();
    for reference in references {
        let Some(file_path) = module_output_paths.get(&reference.module_id) else {
            continue;
        };
        let Some(asset_output_path) = assets_by_logical_path.get(reference.logical_path.as_str())
        else {
            continue;
        };
        rewrites_by_file
            .entry(file_path.clone())
            .or_default()
            .insert(
                reference.logical_path.clone(),
                relative_asset_specifier(file_path.as_str(), asset_output_path),
            );
    }

    for file in &mut project.files {
        let Some(rewrites) = rewrites_by_file.get(file.path.as_str()) else {
            continue;
        };
        file.source =
            rewrite_string_literal_values(file.source.as_str(), file.path.as_str(), rewrites);
    }
}
