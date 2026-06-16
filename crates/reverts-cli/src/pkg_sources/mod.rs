//! Local package source discovery: walking `node_modules` (or a
//! user-supplied root) for a package's directory, parsing its
//! `package.json` into an [`LocalPackageMetadata`] with an
//! [`LocalPackageImportSurface`] (paths + glob patterns), enumerating
//! the local files that belong to the runtime build (skipping tests,
//! `*.d.ts`, `node_modules/`, etc.), and producing
//! [`PackageSource`]s the matcher can score.
//!
//! Everything in this module is pure I/O + path manipulation against
//! the file system. No matcher / connection state required.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use reverts_package_matcher::{PackageSource, is_json_source_path};

use crate::errors::MatchPackagesError;
use crate::{clean_package_entry_path, package_export_specifier};

pub(crate) fn package_dir_candidates(root: &Path, package_name: &str) -> Vec<PathBuf> {
    let package_path = package_name
        .split('/')
        .fold(PathBuf::new(), |path, segment| path.join(segment));
    let candidates = vec![
        root.join("node_modules").join(&package_path),
        root.join(&package_path),
        root.to_path_buf(),
    ];
    let mut seen = BTreeSet::new();
    candidates
        .into_iter()
        .filter(|path| path.is_dir())
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalPackageMetadata {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) import_surface: LocalPackageImportSurface,
}

impl LocalPackageMetadata {
    fn importable_target_for(&self, rel_path: &str) -> Option<LocalPackageImportTarget> {
        if let Some(target) = self.import_surface.paths.get(rel_path) {
            return Some(target.clone());
        }
        let mut pattern_targets = BTreeMap::<String, LocalPackageImportKind>::new();
        for target in self
            .import_surface
            .patterns
            .iter()
            .filter_map(|pattern| pattern.target_for_path(rel_path))
        {
            pattern_targets
                .entry(target.specifier)
                .and_modify(|kind| *kind = kind.merge(target.kind))
                .or_insert(target.kind);
        }
        if pattern_targets.len() == 1 {
            let (specifier, kind) = pattern_targets
                .into_iter()
                .next()
                .expect("one pattern target");
            return Some(LocalPackageImportTarget { specifier, kind });
        }
        if self.import_surface.unrestricted_subpath_imports {
            unrestricted_subpath_import_target(self.name.as_str(), rel_path)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LocalPackageImportSurface {
    pub(crate) paths: BTreeMap<String, LocalPackageImportTarget>,
    pub(crate) patterns: Vec<LocalPackageImportPattern>,
    pub(crate) unrestricted_subpath_imports: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalPackageImportTarget {
    pub(crate) specifier: String,
    pub(crate) kind: LocalPackageImportKind,
}

impl LocalPackageImportTarget {
    const fn esm_external_importable(&self) -> bool {
        self.kind.esm_external_importable()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LocalPackageImportKind {
    Esm,
    CommonJs,
    Universal,
}

impl LocalPackageImportKind {
    const fn esm_external_importable(self) -> bool {
        matches!(self, Self::Esm | Self::Universal)
    }

    const fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Universal, _) | (_, Self::Universal) => Self::Universal,
            (Self::Esm, Self::CommonJs) | (Self::CommonJs, Self::Esm) => Self::Universal,
            (Self::Esm, Self::Esm) => Self::Esm,
            (Self::CommonJs, Self::CommonJs) => Self::CommonJs,
        }
    }

    const fn and_condition(self, condition: Self) -> Option<Self> {
        match (self, condition) {
            (Self::Universal, nested) => Some(nested),
            (parent, Self::Universal) => Some(parent),
            (Self::Esm, Self::Esm) => Some(Self::Esm),
            (Self::CommonJs, Self::CommonJs) => Some(Self::CommonJs),
            (Self::Esm, Self::CommonJs) | (Self::CommonJs, Self::Esm) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct LocalPackageImportPattern {
    pub(crate) target_prefix: String,
    pub(crate) target_suffix: String,
    pub(crate) specifier_prefix: String,
    pub(crate) specifier_suffix: String,
    pub(crate) kind: LocalPackageImportKind,
}

impl LocalPackageImportPattern {
    fn target_for_path(&self, target_path: &str) -> Option<LocalPackageImportTarget> {
        if !target_path.starts_with(self.target_prefix.as_str())
            || !target_path.ends_with(self.target_suffix.as_str())
        {
            return None;
        }
        let wildcard_end = target_path.len().checked_sub(self.target_suffix.len())?;
        if wildcard_end < self.target_prefix.len() {
            return None;
        }
        let wildcard = &target_path[self.target_prefix.len()..wildcard_end];
        if wildcard.is_empty() {
            return None;
        }
        Some(LocalPackageImportTarget {
            specifier: format!(
                "{}{}{}",
                self.specifier_prefix, wildcard, self.specifier_suffix
            ),
            kind: self.kind,
        })
    }
}

pub(crate) fn local_package_metadata(
    package_dir: &Path,
) -> Result<Option<LocalPackageMetadata>, MatchPackagesError> {
    let package_json_path = package_dir.join("package.json");
    if !package_json_path.is_file() {
        return Ok(None);
    }
    let content = fs::read_to_string(package_json_path.as_path()).map_err(|source| {
        MatchPackagesError::ReadPackageSourceRoot {
            path: package_json_path.clone(),
            source,
        }
    })?;
    let value = serde_json::from_str::<serde_json::Value>(content.as_str()).map_err(|source| {
        MatchPackagesError::InvalidPackageMetadata {
            path: package_json_path.clone(),
            source,
        }
    })?;
    let Some(package_name) = value.get("name").and_then(serde_json::Value::as_str) else {
        return Ok(None);
    };
    let Some(package_version) = value
        .get("version")
        .and_then(serde_json::Value::as_str)
        .filter(|version| !version.trim().is_empty())
    else {
        return Ok(None);
    };
    let package_name = package_name.trim().to_string();
    Ok(Some(LocalPackageMetadata {
        import_surface: package_importable_surface(value.as_object(), package_name.as_str()),
        name: package_name,
        version: package_version.trim().to_string(),
    }))
}

pub(crate) fn collect_local_package_sources(
    package_dir: &Path,
    metadata: &LocalPackageMetadata,
    sources: &mut Vec<PackageSource>,
) -> Result<(), MatchPackagesError> {
    let source_files = collect_local_package_source_files(package_dir)?;
    let selected_rel_paths = select_runtime_package_source_paths(&source_files);
    for (rel_path, path) in source_files {
        if !selected_rel_paths.contains(rel_path.as_str()) {
            continue;
        }
        let source = fs::read_to_string(path.as_path()).map_err(|source| {
            MatchPackagesError::ReadPackageSourceRoot {
                path: path.clone(),
                source,
            }
        })?;
        let importable_target = metadata.importable_target_for(rel_path.as_str());
        if is_json_source_path(rel_path.as_str()) && importable_target.is_none() {
            continue;
        }
        let source = package_source_body_for_local_file(rel_path.as_str(), source.as_str())
            .unwrap_or(source);
        let source_path = format!("{}@{}/{}", metadata.name, metadata.version, rel_path);
        if let Some(export_target) = importable_target
            .as_ref()
            .filter(|target| target.esm_external_importable())
        {
            sources.push(PackageSource::external(
                metadata.name.as_str(),
                metadata.version.as_str(),
                export_target.specifier.as_str(),
                source_path,
                source,
            ));
        } else {
            let export_specifier = importable_target
                .as_ref()
                .map(|target| target.specifier.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    package_export_specifier(metadata.name.as_str(), rel_path.as_str())
                });
            sources.push(PackageSource::source_only(
                metadata.name.as_str(),
                metadata.version.as_str(),
                export_specifier,
                source_path,
                source,
            ));
        }
    }
    Ok(())
}

pub(crate) fn collect_local_package_source_files(
    package_dir: &Path,
) -> Result<Vec<(String, PathBuf)>, MatchPackagesError> {
    let mut stack = vec![package_dir.to_path_buf()];
    let mut source_files = Vec::new();
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(dir.as_path()).map_err(|source| {
            MatchPackagesError::ReadPackageSourceRoot {
                path: dir.clone(),
                source,
            }
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| MatchPackagesError::ReadPackageSourceRoot {
                path: dir.clone(),
                source,
            })?;
            let path = entry.path();
            let file_type =
                entry
                    .file_type()
                    .map_err(|source| MatchPackagesError::ReadPackageSourceRoot {
                        path: path.clone(),
                        source,
                    })?;
            if file_type.is_dir() {
                if should_descend_package_source_dir(path.as_path()) {
                    stack.push(path);
                }
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Ok(rel) = path.strip_prefix(package_dir) else {
                continue;
            };
            let rel_path = slash_path(rel);
            if !is_local_package_source_candidate(rel_path.as_str()) {
                continue;
            }
            source_files.push((rel_path, path));
        }
    }
    source_files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(source_files)
}

fn select_runtime_package_source_paths(source_files: &[(String, PathBuf)]) -> BTreeSet<String> {
    let has_compiled_runtime_sources = source_files.iter().any(|(rel_path, _path)| {
        is_javascript_source_path(rel_path)
            && runtime_build_family_score(rel_path)
                .is_some_and(|score| score <= RUNTIME_BUILD_FAMILY_MAX_SCORE)
    });
    source_files
        .iter()
        .filter_map(|(rel_path, _path)| {
            if has_compiled_runtime_sources && is_typescript_source_family_path(rel_path) {
                return None;
            }
            Some(rel_path.clone())
        })
        .collect()
}

fn package_importable_surface(
    package_json: Option<&serde_json::Map<String, serde_json::Value>>,
    package_name: &str,
) -> LocalPackageImportSurface {
    let mut import_surface = LocalPackageImportSurface::default();
    let Some(package_json) = package_json else {
        return import_surface;
    };

    if let Some(exports) = package_json.get("exports") {
        collect_exports_importable_paths(
            exports,
            package_name,
            ".",
            LocalPackageImportKind::Universal,
            &mut import_surface,
        );
        dedup_import_patterns(&mut import_surface.patterns);
        return import_surface;
    }

    // Packages without an `exports` map do not hide their files behind an
    // export whitelist in Node's package resolution. Treat collected runtime
    // files as importable subpaths (`pkg/lib/file.js`) so exact package-source
    // matches can be externalized instead of vendoring the recovered code.
    import_surface.unrestricted_subpath_imports = true;

    if let Some(target) = package_json
        .get("module")
        .and_then(serde_json::Value::as_str)
    {
        insert_importable_exact_target(
            &mut import_surface.paths,
            target,
            package_name,
            LocalPackageImportKind::Esm,
        );
    }
    if let Some(target) = package_json
        .get("browser")
        .and_then(serde_json::Value::as_str)
    {
        insert_importable_exact_target(
            &mut import_surface.paths,
            target,
            package_name,
            LocalPackageImportKind::Esm,
        );
    }
    if let Some(target) = package_json.get("main").and_then(serde_json::Value::as_str) {
        insert_importable_exact_target(
            &mut import_surface.paths,
            target,
            package_name,
            LocalPackageImportKind::Universal,
        );
    }
    insert_importable_exact_target(
        &mut import_surface.paths,
        "index.js",
        package_name,
        LocalPackageImportKind::Universal,
    );
    import_surface
}

fn unrestricted_subpath_import_target(
    package_name: &str,
    rel_path: &str,
) -> Option<LocalPackageImportTarget> {
    let clean = clean_package_entry_path(rel_path);
    if clean.is_empty()
        || clean == "."
        || clean.starts_with("../")
        || clean.contains("/../")
        || clean.ends_with(".d.ts")
    {
        return None;
    }
    let kind = unrestricted_subpath_import_kind(clean.as_str())?;
    Some(LocalPackageImportTarget {
        specifier: package_export_specifier(package_name, clean.as_str()),
        kind,
    })
}

fn unrestricted_subpath_import_kind(rel_path: &str) -> Option<LocalPackageImportKind> {
    match Path::new(rel_path).extension().and_then(|ext| ext.to_str()) {
        Some("mjs" | "ts" | "tsx") => Some(LocalPackageImportKind::Esm),
        Some("cjs") => Some(LocalPackageImportKind::CommonJs),
        Some("js") => Some(LocalPackageImportKind::Universal),
        _ => None,
    }
}

fn collect_exports_importable_paths(
    value: &serde_json::Value,
    package_name: &str,
    export_key: &str,
    kind: LocalPackageImportKind,
    import_surface: &mut LocalPackageImportSurface,
) {
    match value {
        serde_json::Value::String(target) => {
            insert_export_target(import_surface, target, package_name, export_key, kind);
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_exports_importable_paths(
                    item,
                    package_name,
                    export_key,
                    kind,
                    import_surface,
                );
            }
        }
        serde_json::Value::Object(object) => {
            if object.keys().any(|key| key == "." || key.starts_with("./")) {
                for (nested_export_key, nested_value) in object {
                    collect_exports_importable_paths(
                        nested_value,
                        package_name,
                        nested_export_key,
                        kind,
                        import_surface,
                    );
                }
            } else {
                for (condition, nested_kind) in [
                    ("import", LocalPackageImportKind::Esm),
                    ("require", LocalPackageImportKind::CommonJs),
                    ("default", LocalPackageImportKind::Universal),
                    ("node", LocalPackageImportKind::Universal),
                    ("browser", LocalPackageImportKind::Esm),
                ] {
                    if let Some(nested_value) = object.get(condition)
                        && let Some(kind) = kind.and_condition(nested_kind)
                    {
                        collect_exports_importable_paths(
                            nested_value,
                            package_name,
                            export_key,
                            kind,
                            import_surface,
                        );
                    }
                }
            }
        }
        _ => {}
    }
}

fn insert_export_target(
    import_surface: &mut LocalPackageImportSurface,
    target: &str,
    package_name: &str,
    export_key: &str,
    kind: LocalPackageImportKind,
) {
    if export_key.contains('*') || target.contains('*') {
        insert_importable_pattern(
            &mut import_surface.patterns,
            target,
            package_name,
            export_key,
            kind,
        );
        return;
    }
    let Some(export_specifier) = export_key_to_specifier(package_name, export_key) else {
        return;
    };
    insert_importable_exact_target(
        &mut import_surface.paths,
        target,
        export_specifier.as_str(),
        kind,
    );
}

fn export_key_to_specifier(package_name: &str, export_key: &str) -> Option<String> {
    if export_key.contains('*') {
        return None;
    }
    if export_key == "." {
        return Some(package_name.to_string());
    }
    export_key
        .strip_prefix("./")
        .filter(|subpath| !subpath.trim().is_empty())
        .map(|subpath| format!("{package_name}/{subpath}"))
}

fn export_pattern_to_specifier_parts(
    package_name: &str,
    export_key: &str,
) -> Option<(String, String)> {
    if export_key.matches('*').count() != 1 {
        return None;
    }
    let subpath = export_key
        .strip_prefix("./")
        .filter(|subpath| !subpath.trim().is_empty())?;
    let specifier_pattern = format!("{package_name}/{subpath}");
    let (prefix, suffix) = specifier_pattern.split_once('*')?;
    Some((prefix.to_string(), suffix.to_string()))
}

fn insert_importable_exact_target(
    importable_paths: &mut BTreeMap<String, LocalPackageImportTarget>,
    target: &str,
    export_specifier: &str,
    kind: LocalPackageImportKind,
) {
    let Some(clean_target) = clean_export_target(target) else {
        return;
    };
    for candidate in importable_target_candidates(clean_target.as_str()) {
        match importable_paths.get_mut(candidate.as_str()) {
            Some(existing) if existing.specifier == export_specifier => {
                existing.kind = existing.kind.merge(kind);
            }
            Some(_) => {}
            None => {
                importable_paths.insert(
                    candidate,
                    LocalPackageImportTarget {
                        specifier: export_specifier.to_string(),
                        kind,
                    },
                );
            }
        }
    }
}

fn insert_importable_pattern(
    patterns: &mut Vec<LocalPackageImportPattern>,
    target: &str,
    package_name: &str,
    export_key: &str,
    kind: LocalPackageImportKind,
) {
    let Some((specifier_prefix, specifier_suffix)) =
        export_pattern_to_specifier_parts(package_name, export_key)
    else {
        return;
    };
    let Some(clean_target) = clean_export_pattern_target(target) else {
        return;
    };
    let Some((target_prefix, target_suffix)) = clean_target.split_once('*') else {
        return;
    };
    patterns.push(LocalPackageImportPattern {
        target_prefix: target_prefix.to_string(),
        target_suffix: target_suffix.to_string(),
        specifier_prefix,
        specifier_suffix,
        kind,
    });
}

fn clean_export_target(target: &str) -> Option<String> {
    let clean = target
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/');
    if clean.is_empty()
        || clean == "."
        || clean.contains('*')
        || clean.starts_with("../")
        || clean.contains("/../")
    {
        return None;
    }
    Some(clean.to_string())
}

fn clean_export_pattern_target(target: &str) -> Option<String> {
    let clean = target
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/');
    if clean.is_empty()
        || clean == "."
        || clean.matches('*').count() != 1
        || clean.starts_with("../")
        || clean.contains("/../")
    {
        return None;
    }
    Some(clean.to_string())
}

fn importable_target_candidates(clean_target: &str) -> Vec<String> {
    let mut candidates = vec![clean_target.to_string()];
    if Path::new(clean_target).extension().is_none() {
        for extension in ["js", "mjs", "cjs", "ts", "tsx"] {
            candidates.push(format!("{clean_target}.{extension}"));
        }
        for extension in ["js", "mjs", "cjs", "ts", "tsx"] {
            candidates.push(format!("{clean_target}/index.{extension}"));
        }
    }
    candidates
}

fn dedup_import_patterns(patterns: &mut Vec<LocalPackageImportPattern>) {
    let mut seen = BTreeSet::new();
    patterns.retain(|pattern| seen.insert(pattern.clone()));
}

fn should_descend_package_source_dir(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return true;
    };
    !matches!(
        name.to_ascii_lowercase().as_str(),
        "node_modules" | "test" | "tests" | "__tests__" | "coverage" | "benchmark" | "benchmarks"
    )
}

const RUNTIME_BUILD_FAMILY_MAX_SCORE: u8 = 3;

fn runtime_build_family_score(rel_path: &str) -> Option<u8> {
    let lower = rel_path.to_ascii_lowercase();
    let first_segment = lower.split('/').next().unwrap_or("");
    if matches!(
        first_segment,
        "dist" | "lib" | "cjs" | "esm" | "module" | "build"
    ) {
        return Some(1);
    }
    if lower.contains("/dist/")
        || lower.contains("/lib/")
        || lower.contains("/cjs/")
        || lower.contains("/esm/")
    {
        return Some(2);
    }
    if !lower.starts_with("src/") {
        return Some(3);
    }
    Some(5)
}

fn is_typescript_source_family_path(rel_path: &str) -> bool {
    let lower = rel_path.to_ascii_lowercase();
    lower.starts_with("src/")
        && matches!(
            Path::new(lower.as_str())
                .extension()
                .and_then(|ext| ext.to_str()),
            Some("ts" | "tsx")
        )
}

fn is_javascript_source_path(rel_path: &str) -> bool {
    matches!(
        Path::new(rel_path).extension().and_then(|ext| ext.to_str()),
        Some("js" | "mjs" | "cjs")
    )
}

fn package_source_body_for_local_file(rel_path: &str, source: &str) -> Option<String> {
    if is_json_source_path(rel_path) {
        json_package_source_module(source)
    } else {
        None
    }
}

pub(crate) fn json_package_source_module(source: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(source).ok()?;
    let json = serde_json::to_string(&value).ok()?;
    Some(format!("export default {json};\n"))
}

fn is_local_package_source_candidate(rel_path: &str) -> bool {
    let lower = rel_path.to_ascii_lowercase();
    if lower.ends_with(".d.ts")
        || lower.ends_with("tsconfig.json")
        || lower.ends_with("/tsconfig.json")
        || lower.ends_with(".min.js")
        || lower.contains("/test/")
        || lower.contains("/tests/")
        || lower.contains("/__tests__/")
        || lower.starts_with("test/")
        || lower.starts_with("tests/")
        || lower.starts_with("__tests__/")
    {
        return false;
    }
    matches!(
        Path::new(rel_path).extension().and_then(|ext| ext.to_str()),
        Some("js" | "mjs" | "cjs" | "ts" | "tsx" | "json")
    )
}

fn slash_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(segment) => Some(segment.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}
