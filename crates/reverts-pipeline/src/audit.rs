//! Audit passes for the emit pipeline.
//!
//! Three flavours collected here:
//!   * Pre-plan checks (`audit_required_sources`) — make sure every
//!     non-package module the planner needs has either a real source
//!     body or, failing that, surfaces the gap as a warning so the
//!     pipeline doesn't strand the entire project.
//!   * Plan-synthesis checks (`audit_emit_plan_synthesis`,
//!     `audit_file_synthesis`) — verify the planner's intermediate
//!     representation before emission.
//!   * Post-emit checks (`audit_binding_shape_consistency`,
//!     `audit_namespace_object_member_consistency`,
//!     `audit_emitted_project_parse`) — verify the OXC-rendered TS
//!     output against the plan.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use reverts_emitter::EmittedProject;
use reverts_input::InputBundle;
use reverts_ir::{BindingName, BindingShape, ModuleId, ModuleKind};
use reverts_js::{
    DeclarationCallability, ModuleImportExportSurface, ParseGoal, classify_top_level_bindings,
    collect_module_import_export_surface, collect_static_module_specifiers, parse_error_message,
    parse_source,
};
use reverts_model::EnrichedProgram;
use reverts_observe::{AuditFinding, AuditReport, FindingCode};
use reverts_package::PackageResolution;
use reverts_planner::{EmitPlan, PlannedFile};

use crate::EmittedAsset;

pub(crate) fn audit_required_sources(program: &EnrichedProgram) -> AuditReport {
    let mut audit = AuditReport::default();
    for module in program.model().modules() {
        if module.kind == ModuleKind::Package {
            continue;
        }

        if has_module_source(program.model().input(), module.id) {
            continue;
        }

        // A non-package module without a source body means the bundle
        // slice is incomplete for that module. Per ADR 0002 we surface
        // the gap as a warning rather than stranding emission for the
        // whole project; the planner will skip emitting bodies it can't
        // back with source and the audit names the missing binding.
        let definitions = program.model().graph().definitions_for(module.id);
        if definitions.is_empty() {
            audit.push(
                AuditFinding::warning(
                    FindingCode::MissingDefinition,
                    "module has no real source body to emit",
                )
                .with_module(module.id.0.to_string()),
            );
            continue;
        }

        for definition in definitions {
            audit.push(
                AuditFinding::warning(
                    FindingCode::MissingDefinition,
                    "module has symbols but no real source body to emit",
                )
                .with_module(module.id.0.to_string())
                .with_binding(definition.as_str()),
            );
        }
    }
    audit
}

fn has_module_source(input: &InputBundle, module_id: ModuleId) -> bool {
    input.module_source_slice(module_id).is_some()
}

pub(crate) fn audit_emit_plan_synthesis(plan: &EmitPlan) -> AuditReport {
    let mut audit = AuditReport::default();
    for file in &plan.files {
        audit.extend(audit_file_synthesis(file));
    }
    audit
}

/// A recovered module file should be a human-readable unit. When the
/// module-identification / island-splitting mechanism leaves a region unsplit,
/// the emitted file balloons past the line budget. Flag every such file (a
/// Warning — the output still compiles and runs) so the cause can be analyzed
/// and a further split implemented in-pipeline, per the readability contract.
pub(crate) const MAX_MODULE_FILE_LINES: usize = 10_000;

/// True for the mechanical fallback name an island cluster keeps when it never
/// received a semantic name: `…/island/cluster-<token>.ts`, where `<token>` is
/// the cluster's content fingerprint (stable hex) or, for the unreachable
/// `cluster_id` safety-net, decimal digits — both are hex-only and carry no
/// semantic meaning, so either form is an unnamed-island gap. A named cluster's
/// emitted path replaces this with its semantic path. Module naming coverage is
/// tracked separately via `modules.semantic_name` (report coverage), not by path
/// shape, since unnamed modules share the `<id>-esbuild-<token>` form with many
/// legitimate fixtures.
fn is_mechanical_cluster_path(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    let stem = base.strip_suffix(".ts").unwrap_or(base);
    stem.strip_prefix("cluster-").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

/// Flags island clusters that reached output without a semantic name (mechanical
/// `island/cluster-<n>.ts` paths). A Warning, not an Error: the project still
/// builds and runs, but these are naming gaps the completion gate must drive to
/// zero via `name clusters` (keyed by the cluster's stable content fingerprint).
pub(crate) fn audit_unnamed_mechanical_paths(plan: &EmitPlan) -> AuditReport {
    let mut audit = AuditReport::default();
    for file in &plan.files {
        if is_mechanical_cluster_path(&file.path) {
            audit.push(
                AuditFinding::warning(
                    FindingCode::UnnamedMechanicalPath,
                    "island cluster has a mechanical fallback name (no semantic name assigned); \
                     name it via `name clusters`, keyed by its content fingerprint from \
                     .reverts/island-clusters.json"
                        .to_string(),
                )
                .with_module(file.path.clone()),
            );
        }
    }
    audit
}

pub(crate) fn audit_module_file_sizes(plan: &EmitPlan) -> AuditReport {
    let mut audit = AuditReport::default();
    for file in &plan.files {
        // Count emitted physical lines: bodies are stored as line chunks, but a
        // single chunk may itself contain embedded newlines (concatenated
        // imports, minified data), so count newlines across the joined body.
        let body = file.body.join("\n");
        let lines = body.lines().count();
        if lines > MAX_MODULE_FILE_LINES {
            audit.push(
                AuditFinding::warning(
                    FindingCode::OversizedModuleFile,
                    format!(
                        "generated module file has {lines} lines (budget {MAX_MODULE_FILE_LINES}); \
                         analyze the unsplit region and implement a further split in-pipeline"
                    ),
                )
                .with_module(file.path.clone()),
            );
        }
    }
    audit
}

fn audit_file_synthesis(file: &PlannedFile) -> AuditReport {
    let mut audit = AuditReport::default();
    let declarations = planned_declarations(file);

    for binding in &file.bindings {
        if !binding.source_backed {
            audit.push(
                AuditFinding::error(
                    FindingCode::SyntheticReferenceWithoutDeclaration,
                    "planned binding has no recovered source declaration",
                )
                .with_module(file.path.clone())
                .with_binding(binding.emitted.as_str()),
            );
        }
    }

    for import in &file.imports {
        if import.source_backed {
            continue;
        }
        if let PackageResolution::Rejected { specifier, reason } = &import.resolution {
            audit.push(
                AuditFinding::error(
                    FindingCode::UnresolvableBareImport,
                    format!("planned generated import could not be resolved: {reason}"),
                )
                .with_module(file.path.clone())
                .with_binding(format!("{} from {}", import.namespace, specifier)),
            );
        }
    }

    for export in &file.exports {
        if export.source_backed {
            continue;
        }
        if !declarations.contains(&export.binding) {
            audit.push(
                AuditFinding::error(
                    FindingCode::SyntheticReferenceWithoutDeclaration,
                    "planned export references a binding without declaration or import",
                )
                .with_module(file.path.clone())
                .with_binding(export.binding.as_str()),
            );
        }
    }

    audit
}

fn planned_declarations(file: &PlannedFile) -> std::collections::BTreeSet<BindingName> {
    file.imports
        .iter()
        .filter(|import| !import.source_backed)
        .map(|import| import.namespace.clone())
        .chain(file.bindings.iter().map(|binding| binding.emitted.clone()))
        .collect()
}

pub(crate) fn audit_binding_shape_consistency(
    plan: &EmitPlan,
    project: &EmittedProject,
) -> AuditReport {
    let mut audit = AuditReport::default();
    for planned_file in &plan.files {
        let Some(emitted) = project
            .files
            .iter()
            .find(|file| file.path == planned_file.path)
        else {
            continue;
        };
        let classifications = classify_top_level_bindings(
            emitted.source.as_str(),
            Some(Path::new(emitted.path.as_str())),
            ParseGoal::TypeScript,
        );
        for binding in &planned_file.bindings {
            if !binding.source_backed || binding.shape != BindingShape::Callable {
                continue;
            }
            if classifications.get(binding.emitted.as_str())
                == Some(&DeclarationCallability::NotCallable)
            {
                audit.push(
                    AuditFinding::warning(
                        FindingCode::CallableEmittedAsNonCallable,
                        "source-backed binding declared as a non-callable value is called like a function — likely a runtime error in the input",
                    )
                    .with_module(planned_file.path.clone())
                    .with_binding(binding.emitted.as_str()),
                );
            }
        }
    }
    audit
}

/// Paper #7 downstream consumer: for every planned `NamespaceObject`
/// binding, every property name the planner recorded must still appear
/// in the emitted source. Catches refactors that silently strip a member
/// from a namespace surface. Uses a simple whole-word identifier match
/// on the emitted text — false positives would only happen if a member
/// name is shadowed by a literal string of the same name, which would
/// itself be a real concern worth flagging.
pub(crate) fn audit_namespace_object_member_consistency(
    plan: &EmitPlan,
    project: &EmittedProject,
) -> AuditReport {
    let mut audit = AuditReport::default();
    for planned_file in &plan.files {
        let Some(emitted) = project
            .files
            .iter()
            .find(|file| file.path == planned_file.path)
        else {
            continue;
        };
        for binding in &planned_file.bindings {
            if binding.shape != BindingShape::NamespaceObject || binding.known_members.is_empty() {
                continue;
            }
            let namespace = binding.emitted.as_str();
            let missing: Vec<&str> = binding
                .known_members
                .iter()
                .map(|member| member.as_str())
                .filter(|member| {
                    !namespace_member_access_present(emitted.source.as_str(), namespace, member)
                        && !named_import_specifier_present(emitted.source.as_str(), member)
                        && !object_destructure_specifier_present(
                            emitted.source.as_str(),
                            namespace,
                            member,
                        )
                })
                .collect();
            if missing.is_empty() {
                continue;
            }
            audit.push(
                AuditFinding::warning(
                    FindingCode::NamespaceMemberStripped,
                    format!(
                        "namespace binding lost member access for: {}",
                        missing.join(", "),
                    ),
                )
                .with_module(planned_file.path.clone())
                .with_binding(binding.emitted.as_str()),
            );
        }
    }
    audit
}

/// True when `source` contains any of:
///   - `<namespace>.<member>`  (dot access; member must be a valid identifier)
///   - `<namespace>["<member>"]`  (double-quoted computed access)
///   - `<namespace>['<member>']`  (single-quoted computed access)
///
/// Members that are not valid identifiers can only appear through quoted
/// forms; bundlers like esbuild also routinely quote reserved or
/// non-identifier names. Matching any of the three forms keeps the audit
/// from false-firing on those cases.
fn namespace_member_access_present(source: &str, namespace: &str, member: &str) -> bool {
    if is_member_name_identifier(member)
        && contains_identifier(source, &format!("{namespace}.{member}"))
    {
        return true;
    }
    let double_quoted = format!("{namespace}[\"{member}\"]");
    if contains_identifier(source, &double_quoted) {
        return true;
    }
    let single_quoted = format!("{namespace}['{member}']");
    contains_identifier(source, &single_quoted)
}

fn named_import_specifier_present(source: &str, member: &str) -> bool {
    for import_tail in source.split("import ").skip(1) {
        let Some((import_clause, _)) = import_tail.split_once(" from ") else {
            continue;
        };
        let Some(start) = import_clause.find('{') else {
            continue;
        };
        let Some(end) = import_clause[start + 1..].find('}') else {
            continue;
        };
        let named_specifiers = &import_clause[start + 1..start + 1 + end];
        for specifier in named_specifiers.split(',') {
            let specifier = specifier.trim();
            if specifier == member
                || specifier
                    .strip_prefix(member)
                    .is_some_and(|rest| rest.trim_start().starts_with("as "))
                || specifier
                    .rsplit_once(" as ")
                    .is_some_and(|(_, local)| local.trim() == member)
            {
                return true;
            }
        }
    }
    false
}

fn object_destructure_specifier_present(source: &str, namespace: &str, member: &str) -> bool {
    let mut cursor = 0usize;
    while let Some(offset) = source[cursor..].find('=') {
        let equals = cursor + offset;
        cursor = equals + 1;

        let after_equals = source[equals + 1..].trim_start();
        if !starts_with_identifier(after_equals, namespace) {
            continue;
        }

        let before_equals = source[..equals].trim_end();
        let Some(close_brace) = before_equals.rfind('}') else {
            continue;
        };
        if close_brace + 1 != before_equals.len() {
            continue;
        }
        let Some(open_brace) = matching_open_brace_before(before_equals, close_brace) else {
            continue;
        };
        let specifiers = &before_equals[open_brace + 1..close_brace];
        if destructure_specifier_mentions_member(specifiers, member) {
            return true;
        }
    }
    false
}

fn starts_with_identifier(source: &str, identifier: &str) -> bool {
    let Some(rest) = source.strip_prefix(identifier) else {
        return false;
    };
    rest.as_bytes()
        .first()
        .is_none_or(|byte| !is_identifier_part(*byte))
}

fn matching_open_brace_before(source: &str, close_brace: usize) -> Option<usize> {
    let mut depth = 0usize;
    for index in (0..=close_brace).rev() {
        match source.as_bytes()[index] {
            b'}' => depth += 1,
            b'{' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn destructure_specifier_mentions_member(specifiers: &str, member: &str) -> bool {
    specifiers.split(',').any(|specifier| {
        let specifier = specifier.trim();
        specifier == member
            || specifier
                .strip_prefix(member)
                .is_some_and(|rest| rest.trim_start().starts_with(':'))
    })
}

fn is_member_name_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == b'_' || first == b'$') {
        return false;
    }
    bytes.all(is_identifier_part)
}

fn contains_identifier(source: &str, identifier: &str) -> bool {
    let identifier_bytes = identifier.as_bytes();
    if identifier_bytes.is_empty() {
        return false;
    }
    let source_bytes = source.as_bytes();
    let mut cursor = 0;
    while let Some(offset) = source[cursor..].find(identifier) {
        let start = cursor + offset;
        let end = start + identifier_bytes.len();
        let before_ok = start == 0 || !is_identifier_part(source_bytes[start - 1]);
        let after_ok = end >= source_bytes.len() || !is_identifier_part(source_bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        cursor = start + 1;
    }
    false
}

const fn is_identifier_part(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

pub(crate) fn audit_emitted_project_parse(project: &EmittedProject) -> AuditReport {
    let mut audit = AuditReport::default();
    for file in &project.files {
        if let Err(error) = parse_source(
            file.source.as_str(),
            Some(Path::new(file.path.as_str())),
            parse_goal_for_emitted_path(file.path.as_str()),
        ) {
            audit.push(
                AuditFinding::error(
                    FindingCode::UnparseableOutput,
                    parse_error_message(&error, "output could not be parsed"),
                )
                .with_module(file.path.clone()),
            );
        }
    }
    audit
}

pub(crate) fn audit_emitted_relative_import_targets(
    project: &EmittedProject,
    assets: &[EmittedAsset],
    input: &InputBundle,
    module_output_paths: &BTreeMap<ModuleId, String>,
) -> AuditReport {
    let mut audit = AuditReport::default();
    let emitted_paths = project
        .files
        .iter()
        .map(|file| file.path.as_str())
        .chain(assets.iter().map(|asset| asset.path.as_str()))
        .collect::<std::collections::BTreeSet<_>>();
    let source_paths = input
        .source_files
        .iter()
        .map(|source_file| normalize_path(Path::new(source_file.path.as_str())))
        .collect::<BTreeSet<_>>();
    let source_origin_by_output_path =
        source_origin_by_output_path(project, input, module_output_paths);
    for file in &project.files {
        let Ok(specifiers) = collect_static_module_specifiers(
            file.source.as_str(),
            Some(Path::new(file.path.as_str())),
            parse_goal_for_emitted_path(file.path.as_str()),
        ) else {
            continue;
        };
        for specifier in specifiers {
            let specifier = strip_query_and_fragment(specifier.value.as_str());
            if !specifier.starts_with('.') {
                continue;
            }
            if !specifier_requires_emitted_module_target(specifier) {
                continue;
            }
            let candidates =
                emitted_relative_import_target_candidates(file.path.as_str(), specifier);
            if candidates
                .iter()
                .any(|candidate| emitted_paths.contains(candidate.as_str()))
            {
                continue;
            }
            let Some(source_origin) = source_origin_by_output_path.get(file.path.as_str()) else {
                continue;
            };
            if !source_import_candidates(
                normalize_join(
                    Path::new(source_origin)
                        .parent()
                        .unwrap_or_else(|| Path::new("")),
                    specifier,
                )
                .as_str(),
            )
            .iter()
            .any(|candidate| source_paths.contains(candidate))
            {
                continue;
            }
            audit.push(
                AuditFinding::error(
                    FindingCode::UnresolvableBareImport,
                    "emitted relative module specifier does not resolve to an emitted file",
                )
                .with_module(file.path.clone())
                .with_binding(specifier.to_string()),
            );
        }
    }
    audit
}

/// Verify every first-party NAMED import resolves to a matching export in its
/// target module — esbuild's `No matching export`, caught deterministically
/// in-pipeline. `audit_emitted_relative_import_targets` already proves the
/// target FILE exists; this proves the imported wire NAME is actually exported
/// there. A dangling name is our decompiler's defect (a wire rename collapsed an
/// export without every importer following), so it blocks output (Error) with
/// the exact importer / name / target rather than failing cryptically at bundle.
///
/// Conservative by construction — it never flags what it cannot prove dangling:
///   * a target with a bare `export * from '…'` re-exports an opaque set, so it
///     is treated as exporting anything;
///   * a target that failed to parse yields no surface and is skipped;
///   * a specifier that resolves to no emitted file is left to the file-target
///     audit;
///   * type-only imports/exports bind no runtime value and are ignored.
pub(crate) fn audit_emitted_named_export_consistency(project: &EmittedProject) -> AuditReport {
    let mut audit = AuditReport::default();
    // Parse each emitted file once into its import/export surface.
    let mut surfaces: BTreeMap<&str, ModuleImportExportSurface> = BTreeMap::new();
    for file in &project.files {
        if let Ok(surface) = collect_module_import_export_surface(
            file.source.as_str(),
            Some(Path::new(file.path.as_str())),
            parse_goal_for_emitted_path(file.path.as_str()),
        ) {
            surfaces.insert(file.path.as_str(), surface);
        }
    }
    let emitted_paths = project
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<BTreeSet<_>>();
    for file in &project.files {
        let Some(surface) = surfaces.get(file.path.as_str()) else {
            continue;
        };
        for edge in &surface.named_imports {
            let specifier = strip_query_and_fragment(edge.specifier.as_str());
            if !specifier.starts_with('.') {
                continue; // bare/package import — not a first-party module
            }
            if !specifier_requires_emitted_module_target(specifier) {
                continue;
            }
            let candidates =
                emitted_relative_import_target_candidates(file.path.as_str(), specifier);
            let Some(target_path) = candidates
                .iter()
                .find(|candidate| emitted_paths.contains(candidate.as_str()))
            else {
                continue; // unresolved file target — the file-target audit owns it
            };
            let Some(target_surface) = surfaces.get(target_path.as_str()) else {
                continue; // target failed to parse — cannot prove anything
            };
            if target_surface.has_export_star {
                continue; // opaque re-export — an unlisted name may still resolve
            }
            for name in &edge.imported_names {
                if !target_surface.exported_names.contains(name) {
                    audit.push(
                        AuditFinding::error(
                            FindingCode::DanglingNamedImport,
                            format!(
                                "import {{ {name} }} from '{specifier}' has no matching export in \
                                 target module '{target_path}'"
                            ),
                        )
                        .with_module(file.path.clone())
                        .with_binding(name.clone()),
                    );
                }
            }
        }
    }
    audit
}

fn source_origin_by_output_path(
    project: &EmittedProject,
    input: &InputBundle,
    module_output_paths: &BTreeMap<ModuleId, String>,
) -> BTreeMap<String, String> {
    let source_paths_by_id = input
        .source_files
        .iter()
        .map(|source_file| (source_file.id, source_file.path.as_str()))
        .collect::<BTreeMap<_, _>>();
    let mut origins = BTreeMap::<String, String>::new();
    for module in &input.modules {
        let Some(output_path) = module_output_paths.get(&module.id) else {
            continue;
        };
        let Some(source_file_id) = module.source_file_id else {
            continue;
        };
        let Some(source_path) = source_paths_by_id.get(&source_file_id) else {
            continue;
        };
        origins.insert(output_path.clone(), (*source_path).to_string());
    }
    for file in &project.files {
        if let Some(source_path) = preserved_source_import_target(file.source.as_str()) {
            origins.insert(file.path.clone(), source_path.to_string());
        }
    }
    origins
}

fn preserved_source_import_target(source: &str) -> Option<&str> {
    source
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("// reverts-preserved-source-import-target: "))
}

fn parse_goal_for_emitted_path(path: &str) -> ParseGoal {
    match Path::new(path)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
    {
        Some("js" | "jsx" | "mjs" | "cjs") => ParseGoal::JavaScript,
        _ => ParseGoal::TypeScript,
    }
}

fn specifier_requires_emitted_module_target(specifier: &str) -> bool {
    match Path::new(specifier)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
    {
        Some("js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts") | None => true,
        Some(_) => false,
    }
}

fn source_import_candidates(base: &str) -> Vec<String> {
    let mut candidates = vec![base.to_string()];
    if Path::new(base).extension().is_none() {
        candidates.extend(
            [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"].map(|ext| format!("{base}{ext}")),
        );
        candidates.extend(
            ["index.ts", "index.tsx", "index.js", "index.jsx"].map(|name| format!("{base}/{name}")),
        );
    }
    candidates
}

fn emitted_relative_import_target_candidates(from_file: &str, specifier: &str) -> Vec<String> {
    let base = normalize_join(
        Path::new(from_file)
            .parent()
            .unwrap_or_else(|| Path::new("")),
        specifier,
    );
    let mut candidates = vec![base.clone()];
    let path = Path::new(base.as_str());
    match path.extension().and_then(std::ffi::OsStr::to_str) {
        Some("js") => candidates.push(replace_extension(base.as_str(), "ts")),
        Some("jsx") => candidates.push(replace_extension(base.as_str(), "tsx")),
        Some("mjs") => candidates.push(replace_extension(base.as_str(), "mts")),
        Some("cjs") => candidates.push(replace_extension(base.as_str(), "cts")),
        None => {
            candidates.extend(
                [".ts", ".tsx", ".js", ".jsx", ".mts", ".cts"]
                    .map(|extension| format!("{base}{extension}")),
            );
            candidates
                .extend(["index.ts", "index.tsx", "index.js"].map(|name| format!("{base}/{name}")));
        }
        _ => {}
    }
    candidates
}

fn replace_extension(path: &str, extension: &str) -> String {
    let mut replaced = Path::new(path).to_path_buf();
    replaced.set_extension(extension);
    normalize_path(replaced.as_path())
}

fn normalize_join(base: &Path, specifier: &str) -> String {
    normalize_path(base.join(specifier).as_path())
}

fn normalize_path(path: &Path) -> String {
    let mut parts = Vec::<String>::new();
    let mut absolute = false;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => parts.push(prefix.as_os_str().to_string_lossy().into()),
            Component::RootDir => absolute = true,
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.last().is_some_and(|part| part != "..") {
                    parts.pop();
                } else {
                    parts.push("..".to_string());
                }
            }
            Component::Normal(part) => parts.push(part.to_string_lossy().into()),
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

fn strip_query_and_fragment(value: &str) -> &str {
    let query_index = value.find('?').unwrap_or(value.len());
    let fragment_index = value.find('#').unwrap_or(value.len());
    &value[..query_index.min(fragment_index)]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use reverts_emitter::{EmittedFile, EmittedProject};
    use reverts_input::{InputBundle, InputRows, ModuleInput, ProjectInput, SourceFileInput};
    use reverts_ir::ModuleId;
    use reverts_observe::FindingCode;

    use crate::EmittedAsset;

    fn empty_input() -> InputBundle {
        InputBundle::from_rows(InputRows::new(ProjectInput::new(1, "fixture")))
            .expect("empty fixture input should be valid")
    }

    fn relative_input() -> (InputBundle, BTreeMap<ModuleId, String>) {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "/app/a.js",
            Some("import { b } from './b.js';".into()),
        ));
        rows.source_files.push(SourceFileInput::new(
            2,
            "/app/b.js",
            Some("export const b = 1;".into()),
        ));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "a", "modules/a.ts").with_source_file(1));
        (
            InputBundle::from_rows(rows).expect("relative fixture input should be valid"),
            BTreeMap::from([(ModuleId(1), "modules/a.ts".to_string())]),
        )
    }

    #[test]
    fn relative_js_import_resolves_to_emitted_typescript_source() {
        let (input, module_output_paths) = relative_input();
        let project = EmittedProject {
            files: vec![
                EmittedFile {
                    path: "modules/a.ts".into(),
                    source: "import { b } from './b.js'; export const a = b;".into(),
                },
                EmittedFile {
                    path: "modules/b.ts".into(),
                    source: "export const b = 1;".into(),
                },
            ],
        };

        let audit = super::audit_emitted_relative_import_targets(
            &project,
            &[],
            &input,
            &module_output_paths,
        );

        assert!(!audit.has(FindingCode::UnresolvableBareImport));
    }

    #[test]
    fn relative_native_import_resolves_to_emitted_asset() {
        let input = empty_input();
        let project = EmittedProject {
            files: vec![EmittedFile {
                path: "entrypoint.ts".into(),
                source: "import './crypto/build/Release/sshcrypto.node';".into(),
            }],
        };
        let assets = vec![EmittedAsset {
            path: "crypto/build/Release/sshcrypto.node".into(),
            bytes: vec![1, 2, 3],
            executable: false,
        }];

        let audit = super::audit_emitted_relative_import_targets(
            &project,
            &assets,
            &input,
            &BTreeMap::new(),
        );

        assert!(!audit.has(FindingCode::UnresolvableBareImport));
    }

    #[test]
    fn relative_native_import_is_not_a_module_target_error() {
        let input = empty_input();
        let project = EmittedProject {
            files: vec![EmittedFile {
                path: "entrypoint.ts".into(),
                source: "import './crypto/build/Release/sshcrypto.node';".into(),
            }],
        };

        let audit =
            super::audit_emitted_relative_import_targets(&project, &[], &input, &BTreeMap::new());

        assert!(!audit.has(FindingCode::UnresolvableBareImport));
    }

    #[test]
    fn missing_relative_import_target_is_an_error() {
        let (input, module_output_paths) = relative_input();
        let project = EmittedProject {
            files: vec![EmittedFile {
                path: "modules/a.ts".into(),
                source: "import { b } from './b.js'; export const a = b;".into(),
            }],
        };

        let audit = super::audit_emitted_relative_import_targets(
            &project,
            &[],
            &input,
            &module_output_paths,
        );

        assert!(audit.has(FindingCode::UnresolvableBareImport));
        assert!(audit.has_errors());
    }

    #[test]
    fn unknown_relative_import_target_is_not_a_known_source_error() {
        let input = empty_input();
        let project = EmittedProject {
            files: vec![EmittedFile {
                path: "src/bundle.ts".into(),
                source: "const value = require('./foo');".into(),
            }],
        };

        let audit =
            super::audit_emitted_relative_import_targets(&project, &[], &input, &BTreeMap::new());

        assert!(!audit.has(FindingCode::UnresolvableBareImport));
    }

    #[test]
    fn oversized_module_file_is_flagged_as_a_warning() {
        use reverts_planner::{EmitPlan, PlannedFile};
        let mut plan = EmitPlan::default();
        let mut big = PlannedFile::new("modules/island/cluster-huge.ts");
        // One statement per line, past the budget.
        let body = (0..super::MAX_MODULE_FILE_LINES + 5)
            .map(|i| format!("var v{i} = {i};"))
            .collect::<Vec<_>>()
            .join("\n");
        big.push_source(body);
        plan.push_file(big);
        let mut ok = PlannedFile::new("modules/small.ts");
        ok.push_source("var a = 1;\nvar b = 2;\n");
        plan.push_file(ok);

        let audit = super::audit_module_file_sizes(&plan);
        assert!(audit.has(FindingCode::OversizedModuleFile));
        // Exactly the oversized file is flagged, and it is a Warning (not an
        // Error that would block output — the file still compiles and runs).
        assert_eq!(audit.warning_count(), 1);
        assert_eq!(audit.error_count(), 0);
    }

    #[test]
    fn unnamed_island_clusters_are_flagged_as_warnings() {
        use reverts_planner::{EmitPlan, PlannedFile};
        let mut plan = EmitPlan::default();
        for path in [
            "modules/island/cluster-42.ts",   // legacy id-form (digits)  -> flag
            "modules/island/cluster-2875.ts", // legacy id-form          -> flag
            "modules/island/cluster-00000000000004a2.ts", // fingerprint  -> flag
            "modules/island/cluster-deadbeefcafef00d.ts", // fingerprint  -> flag
            "modules/island/auth/oauth-token-flow.ts", // named cluster path -> ok
            "modules/island/cluster-huge.ts", // suffix not all-hex      -> ok
            "modules/island/vendor/zod-checks.ts", // vendor-relocated    -> ok
            "modules/388-esbuild-XY.ts",      // module (tracked via DB) -> ok
        ] {
            let mut file = PlannedFile::new(path);
            file.push_source("var a = 1;\n");
            plan.push_file(file);
        }
        let audit = super::audit_unnamed_mechanical_paths(&plan);
        assert!(audit.has(FindingCode::UnnamedMechanicalPath));
        // The two legacy id-form + two fingerprint-form clusters, all Warnings.
        assert_eq!(audit.warning_count(), 4);
        assert_eq!(audit.error_count(), 0);
    }

    #[test]
    fn within_budget_module_files_are_not_flagged() {
        use reverts_planner::{EmitPlan, PlannedFile};
        let mut plan = EmitPlan::default();
        let mut file = PlannedFile::new("modules/m.ts");
        let body = (0..super::MAX_MODULE_FILE_LINES - 1)
            .map(|i| format!("var v{i} = {i};"))
            .collect::<Vec<_>>()
            .join("\n");
        file.push_source(body);
        plan.push_file(file);

        let audit = super::audit_module_file_sizes(&plan);
        assert!(!audit.has(FindingCode::OversizedModuleFile));
    }

    fn project(files: &[(&str, &str)]) -> EmittedProject {
        EmittedProject {
            files: files
                .iter()
                .map(|(path, source)| EmittedFile {
                    path: (*path).into(),
                    source: (*source).into(),
                })
                .collect(),
        }
    }

    #[test]
    fn named_import_with_matching_export_is_clean() {
        // Shorthand, aliased export, and re-export-chain forms all resolve.
        let project = project(&[
            (
                "modules/a.ts",
                "import { b } from './b.js';\nimport { renamed } from './c.js';\nexport const a = b + renamed;",
            ),
            ("modules/b.ts", "export const b = 1;"),
            // `export { local as renamed }` — the PUBLIC name is `renamed`.
            (
                "modules/c.ts",
                "const local = 2;\nexport { local as renamed };",
            ),
        ]);

        let audit = super::audit_emitted_named_export_consistency(&project);
        assert!(!audit.has(FindingCode::DanglingNamedImport));
    }

    #[test]
    fn named_import_without_matching_export_is_an_error() {
        // `gone` is not exported by the target — esbuild's `No matching export`.
        let project = project(&[
            (
                "modules/a.ts",
                "import { gone } from './b.js';\nexport const a = gone;",
            ),
            ("modules/b.ts", "export const present = 1;"),
        ]);

        let audit = super::audit_emitted_named_export_consistency(&project);
        assert!(audit.has(FindingCode::DanglingNamedImport));
        assert!(audit.has_errors());
        assert_eq!(audit.error_count(), 1);
        let finding = &audit.findings()[0];
        assert_eq!(finding.binding.as_deref(), Some("gone"));
        assert_eq!(finding.module.as_deref(), Some("modules/a.ts"));
    }

    #[test]
    fn export_star_target_suppresses_the_check() {
        // A bare `export *` re-exports an opaque set, so an unlisted name may
        // still resolve through it — never flag it.
        let project = project(&[
            (
                "modules/a.ts",
                "import { anything } from './b.js';\nexport const a = anything;",
            ),
            ("modules/b.ts", "export * from './deep.js';"),
        ]);

        let audit = super::audit_emitted_named_export_consistency(&project);
        assert!(!audit.has(FindingCode::DanglingNamedImport));
    }

    #[test]
    fn unresolved_file_target_is_left_to_the_file_audit() {
        // The target file is not emitted at all — `audit_emitted_relative_import_targets`
        // owns that, so this audit stays quiet (no double report).
        let project = project(&[(
            "modules/a.ts",
            "import { x } from './missing.js';\nexport const a = x;",
        )]);

        let audit = super::audit_emitted_named_export_consistency(&project);
        assert!(!audit.has(FindingCode::DanglingNamedImport));
    }

    #[test]
    fn namespace_and_bare_imports_are_not_checked() {
        // `import * as ns` and bare/package imports demand no specific wire name.
        let project = project(&[
            (
                "modules/a.ts",
                "import * as ns from './b.js';\nimport { readFile } from 'node:fs';\nexport const a = ns;",
            ),
            ("modules/b.ts", "export const only = 1;"),
        ]);

        let audit = super::audit_emitted_named_export_consistency(&project);
        assert!(!audit.has(FindingCode::DanglingNamedImport));
    }
}
