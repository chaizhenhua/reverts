use std::collections::{BTreeMap, BTreeSet};

use reverts_graph::{AstFactKind, AstWrapperKind};
use reverts_input::ModuleDependencyTarget;
use reverts_ir::{BindingConstraintKind, BindingName, BindingShapeSolution, ModuleId};
use reverts_js::sanitize_identifier;
use reverts_model::{
    CompilerEvidence, CompilerKind, CompilerProfile, EnrichedProgram, ModuleCompilerProfile,
    PackageImportDecision, ProgramModel, SemanticNameMap,
};
use reverts_observe::{AuditFinding, AuditReport, FindingCode};
use reverts_package::{PackageResolution, PackageSurfaceIndex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrichmentOutput {
    pub program: EnrichedProgram,
    pub audit: AuditReport,
}

#[must_use]
pub fn enrich_program(model: ProgramModel) -> EnrichmentOutput {
    let semantic_names = assign_semantic_names(&model);
    let binding_shapes = BindingShapeSolution::from_def_use_graph(model.graph().def_use());
    let compiler_profile = detect_compiler_profile(&model);
    let package_index = PackageSurfaceIndex::from_attributions(
        model.input().package_attributions.as_slice(),
        model.input().package_surfaces.as_slice(),
    );
    let mut audit = AuditReport::default();
    audit.extend(audit_ast_fact_extraction(&model));
    audit.extend(audit_def_use_graph(&model));
    audit.extend(audit_duplicate_top_level_bindings(&model));
    audit.extend(audit_binding_shape_conflicts(&binding_shapes));
    let package_imports = resolve_package_imports(&model, &package_index, &mut audit);

    EnrichmentOutput {
        program: EnrichedProgram::new(model, semantic_names, package_imports, binding_shapes)
            .with_compiler_profile(compiler_profile),
        audit,
    }
}

fn audit_ast_fact_extraction(model: &ProgramModel) -> AuditReport {
    let mut audit = AuditReport::default();
    for error in model.graph().ast_errors() {
        audit.push(
            AuditFinding::error(FindingCode::AstFactExtractionFailed, error.message.clone())
                .with_module(error.module_id.0.to_string())
                .with_binding(error.path.clone()),
        );
    }
    audit
}

fn audit_def_use_graph(model: &ProgramModel) -> AuditReport {
    let mut audit = AuditReport::default();
    for (module_id, binding) in model.graph().def_use().unresolved_reads() {
        if is_ambient_binding(binding.as_str()) {
            continue;
        }
        audit.push(
            AuditFinding::error(
                FindingCode::MissingDefinition,
                format!("binding '{binding}' is read without a local definition or import"),
            )
            .with_module(module_id.0.to_string())
            .with_binding(binding.as_str()),
        );
    }
    for (module_id, binding) in model.graph().def_use().unresolved_writes() {
        if is_ambient_binding(binding.as_str()) {
            continue;
        }
        audit.push(
            AuditFinding::error(
                FindingCode::MissingDefinition,
                format!("binding '{binding}' is written without a local definition or import"),
            )
            .with_module(module_id.0.to_string())
            .with_binding(binding.as_str()),
        );
    }
    audit
}

fn audit_duplicate_top_level_bindings(model: &ProgramModel) -> AuditReport {
    let mut counts = BTreeMap::<(ModuleId, BindingName), usize>::new();
    for fact in model
        .graph()
        .ast_facts()
        .iter()
        .filter(|fact| fact.kind == AstFactKind::Definition)
    {
        let Some(binding) = &fact.binding else {
            continue;
        };
        *counts.entry((fact.module_id, binding.clone())).or_default() += 1;
    }

    let mut audit = AuditReport::default();
    for ((module_id, binding), count) in counts {
        if count <= 1 {
            continue;
        }
        audit.push(
            AuditFinding::error(
                FindingCode::DuplicateTopLevelBinding,
                format!("top-level binding '{binding}' is declared {count} times"),
            )
            .with_module(module_id.0.to_string())
            .with_binding(binding.as_str()),
        );
    }
    audit
}

fn is_ambient_binding(binding: &str) -> bool {
    matches!(
        binding,
        "Array"
            | "ArrayBuffer"
            | "Atomics"
            | "AbortController"
            | "AsyncContext"
            | "BigInt"
            | "BigInt64Array"
            | "BigUint64Array"
            | "Blob"
            | "Boolean"
            | "Buffer"
            | "DOMParser"
            | "DataView"
            | "Date"
            | "Error"
            | "EvalError"
            | "File"
            | "FileReader"
            | "Float32Array"
            | "Float64Array"
            | "FormData"
            | "Function"
            | "Headers"
            | "Infinity"
            | "Intl"
            | "JSON"
            | "Map"
            | "Math"
            | "MessageChannel"
            | "NaN"
            | "Number"
            | "Object"
            | "OffscreenCanvas"
            | "Promise"
            | "Proxy"
            | "RangeError"
            | "ReferenceError"
            | "Reflect"
            | "RegExp"
            | "Request"
            | "Response"
            | "Screen"
            | "Set"
            | "String"
            | "Symbol"
            | "SyntaxError"
            | "TextDecoder"
            | "TextEncoder"
            | "TypeError"
            | "URIError"
            | "URL"
            | "URLSearchParams"
            | "Uint8Array"
            | "Uint8ClampedArray"
            | "Uint16Array"
            | "Uint32Array"
            | "WeakMap"
            | "WeakSet"
            | "__dirname"
            | "__filename"
            | "atob"
            | "browser"
            | "btoa"
            | "chrome"
            | "clearImmediate"
            | "clearInterval"
            | "clearTimeout"
            | "console"
            | "crypto"
            | "document"
            | "exports"
            | "fetch"
            | "global"
            | "globalThis"
            | "localStorage"
            | "location"
            | "module"
            | "navigator"
            | "performance"
            | "process"
            | "queueMicrotask"
            | "require"
            | "self"
            | "setImmediate"
            | "setInterval"
            | "setTimeout"
            | "structuredClone"
            | "undefined"
            | "window"
            | "XMLHttpRequest"
    )
}

fn audit_binding_shape_conflicts(binding_shapes: &BindingShapeSolution) -> AuditReport {
    let mut audit = AuditReport::default();
    for conflict in binding_shapes.conflicts() {
        audit.push(
            AuditFinding::error(
                binding_shape_conflict_code(conflict.existing_kind, conflict.incoming_kind),
                format!(
                    "binding has incompatible shape constraints: {:?} requires {:?}, {:?} requires {:?}",
                    conflict.existing_kind,
                    conflict.existing_shape,
                    conflict.incoming_kind,
                    conflict.incoming_shape
                ),
            )
            .with_module(conflict.module_id.0.to_string())
            .with_binding(conflict.binding.as_str().to_string()),
        );
    }
    audit
}

fn binding_shape_conflict_code(
    existing_kind: BindingConstraintKind,
    incoming_kind: BindingConstraintKind,
) -> FindingCode {
    if matches!(
        (existing_kind, incoming_kind),
        (
            BindingConstraintKind::ObjectLiteralDeclaration,
            BindingConstraintKind::Call
        ) | (
            BindingConstraintKind::Call,
            BindingConstraintKind::ObjectLiteralDeclaration
        )
    ) {
        FindingCode::CallableEmittedAsNonCallable
    } else {
        FindingCode::AmbiguousBindingShape
    }
}

fn assign_semantic_names(model: &ProgramModel) -> SemanticNameMap {
    let mut semantic_names = SemanticNameMap::default();
    let mut used_by_module: BTreeMap<ModuleId, BTreeSet<String>> = BTreeMap::new();
    let mut mapped_originals = BTreeSet::<(ModuleId, String)>::new();

    for module in model.modules() {
        semantic_names.insert_module_path(module.id, module.semantic_path.clone());
    }

    for symbol in model.symbols() {
        if !mapped_originals.insert((symbol.module_id, symbol.name.clone())) {
            continue;
        }
        let naming_hint = symbol
            .semantic_name
            .as_deref()
            .or(symbol.export_name.as_deref())
            .unwrap_or(symbol.name.as_str());
        let base = sanitize_identifier(naming_hint);
        let semantic = reserve_unique_name(&mut used_by_module, symbol.module_id, &base);
        semantic_names.insert_binding(symbol.module_id, symbol.name.clone(), semantic);
    }

    semantic_names
}

fn reserve_unique_name(
    used_by_module: &mut BTreeMap<ModuleId, BTreeSet<String>>,
    module_id: ModuleId,
    base: &str,
) -> String {
    let used = used_by_module.entry(module_id).or_default();
    if used.insert(base.to_string()) {
        return base.to_string();
    }

    let mut suffix = 2_u32;
    loop {
        let candidate = format!("{base}_{suffix}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        suffix += 1;
    }
}

fn detect_compiler_profile(model: &ProgramModel) -> CompilerProfile {
    let mut identifiers_by_module = BTreeMap::<ModuleId, BTreeSet<String>>::new();
    let mut wrappers_by_module = BTreeMap::<ModuleId, BTreeSet<AstWrapperKind>>::new();
    for fact in model.graph().ast_facts() {
        if let Some(binding) = &fact.binding {
            identifiers_by_module
                .entry(fact.module_id)
                .or_default()
                .insert(binding.as_str().to_string());
        }
        if let AstFactKind::WrapperRegion(kind) = fact.kind {
            wrappers_by_module
                .entry(fact.module_id)
                .or_default()
                .insert(kind);
        }
    }

    let mut profile = CompilerProfile::default();
    for module in model.modules() {
        let Some(source) = model.input().module_source_slice(module.id) else {
            continue;
        };
        let identifiers = identifiers_by_module
            .get(&module.id)
            .cloned()
            .unwrap_or_default();
        let wrappers = wrappers_by_module
            .get(&module.id)
            .cloned()
            .unwrap_or_default();
        profile.insert_module(
            module.id,
            detect_module_compiler_profile(source.source, &identifiers, &wrappers),
        );
    }
    profile
}

fn detect_module_compiler_profile(
    source: &str,
    identifiers: &BTreeSet<String>,
    wrappers: &BTreeSet<AstWrapperKind>,
) -> ModuleCompilerProfile {
    let mut evidence = Vec::new();
    let minified = looks_minified(source);
    if minified {
        evidence.push(CompilerEvidence::MinifiedLayout);
    }
    for wrapper in wrappers {
        evidence.push(CompilerEvidence::TopLevelIife(*wrapper));
    }

    let compiler =
        if collect_identifier_evidence(identifiers, WEBPACK_RUNTIME_IDENTIFIERS, &mut evidence) {
            CompilerKind::Webpack
        } else if collect_identifier_evidence(
            identifiers,
            ESBUILD_RUNTIME_IDENTIFIERS,
            &mut evidence,
        ) {
            CompilerKind::Esbuild
        } else if collect_identifier_evidence(
            identifiers,
            ROLLUP_RUNTIME_IDENTIFIERS,
            &mut evidence,
        ) || collect_source_pattern_evidence(
            source,
            ROLLUP_SOURCE_PATTERNS,
            &mut evidence,
        ) {
            CompilerKind::Rollup
        } else if collect_identifier_evidence(identifiers, BABEL_RUNTIME_IDENTIFIERS, &mut evidence)
        {
            CompilerKind::Babel
        } else if minified {
            CompilerKind::Terser
        } else {
            CompilerKind::Unknown
        };

    ModuleCompilerProfile::new(compiler, minified, evidence)
}

fn collect_identifier_evidence(
    identifiers: &BTreeSet<String>,
    candidates: &[&'static str],
    evidence: &mut Vec<CompilerEvidence>,
) -> bool {
    let mut matched = false;
    for candidate in candidates {
        if identifiers.contains(*candidate) {
            evidence.push(CompilerEvidence::Identifier((*candidate).to_string()));
            matched = true;
        }
    }
    matched
}

fn collect_source_pattern_evidence(
    source: &str,
    candidates: &[&'static str],
    evidence: &mut Vec<CompilerEvidence>,
) -> bool {
    let mut matched = false;
    for candidate in candidates {
        if source.contains(*candidate) {
            evidence.push(CompilerEvidence::SourcePattern(candidate));
            matched = true;
        }
    }
    matched
}

fn looks_minified(source: &str) -> bool {
    let byte_len = source.len();
    if byte_len < 120 {
        return false;
    }

    let non_empty_lines = source
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    if non_empty_lines.is_empty() {
        return false;
    }

    let max_line_len = non_empty_lines
        .iter()
        .map(|line| line.len())
        .max()
        .unwrap_or(0);
    let whitespace_count = source.bytes().filter(u8::is_ascii_whitespace).count();
    let whitespace_ratio = whitespace_count as f64 / byte_len as f64;
    let average_line_len = byte_len / non_empty_lines.len();

    (non_empty_lines.len() == 1 && max_line_len >= 120)
        || (average_line_len >= 160 && whitespace_ratio <= 0.12)
}

const WEBPACK_RUNTIME_IDENTIFIERS: &[&str] = &[
    "__webpack_require__",
    "__webpack_exports__",
    "__webpack_modules__",
    "__webpack_module_cache__",
    "webpackChunk",
    "webpackJsonp",
];

const ESBUILD_RUNTIME_IDENTIFIERS: &[&str] = &[
    "__defProp",
    "__export",
    "__copyProps",
    "__toESM",
    "__toCommonJS",
    "__commonJS",
    "__require",
];

const ROLLUP_RUNTIME_IDENTIFIERS: &[&str] = &[
    "commonjsGlobal",
    "getDefaultExportFromCjs",
    "getAugmentedNamespace",
    "_mergeNamespaces",
    "_interopNamespaceDefault",
    "_interopDefaultLegacy",
];

const ROLLUP_SOURCE_PATTERNS: &[&str] = &["Object.freeze"];

const BABEL_RUNTIME_IDENTIFIERS: &[&str] = &[
    "_interopRequireDefault",
    "_interopRequireWildcard",
    "_classCallCheck",
    "_createClass",
    "_defineProperty",
    "_inherits",
    "_possibleConstructorReturn",
    "regeneratorRuntime",
];

fn resolve_package_imports(
    model: &ProgramModel,
    package_index: &PackageSurfaceIndex,
    audit: &mut AuditReport,
) -> Vec<PackageImportDecision> {
    let mut requested_imports = BTreeMap::<(ModuleId, String), bool>::new();
    for dependency in &model.input().dependencies {
        let ModuleDependencyTarget::Package { specifier } = &dependency.target else {
            continue;
        };
        requested_imports
            .entry((dependency.from_module_id, specifier.clone()))
            .or_insert(false);
    }

    for fact in model
        .graph()
        .ast_facts()
        .iter()
        .filter(|fact| fact.kind == AstFactKind::PackageImport)
    {
        let Some(specifier) = &fact.binding else {
            continue;
        };
        requested_imports.insert((fact.module_id, specifier.as_str().to_string()), true);
    }

    let mut decisions = Vec::new();

    for ((from_module_id, specifier), source_backed) in requested_imports {
        let resolution = package_index.resolve(&specifier);
        if let PackageResolution::Rejected { reason, .. } = &resolution {
            audit.push(
                AuditFinding::error(FindingCode::UnresolvableBareImport, reason.clone())
                    .with_module(from_module_id.0.to_string())
                    .with_binding(specifier.clone()),
            );
        }

        decisions.push(PackageImportDecision::with_source_backed(
            from_module_id,
            BindingName::new(package_namespace_binding(&specifier)),
            resolution,
            source_backed,
        ));
    }

    decisions
}

fn package_namespace_binding(specifier: &str) -> String {
    let sanitized = sanitize_identifier(specifier);
    format!("__pkg_{sanitized}")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use reverts_input::{
        InputBundle, InputRows, ModuleDependencyInput, ModuleDependencyTarget, ModuleInput,
        PackageAttributionInput, PackageSurfaceInput, ProjectInput, SourceFileInput, SymbolInput,
    };
    use reverts_ir::ModuleId;
    use reverts_model::CompilerKind;
    use reverts_observe::FindingCode;
    use reverts_package::PackageResolution;

    use super::{ProgramModel, detect_module_compiler_profile, enrich_program};

    fn valid_rows() -> InputRows {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "app", "src/index.ts"));
        rows
    }

    #[test]
    fn accepted_attribution_resolves_package_dependency() {
        let mut rows = valid_rows();
        rows.modules.push(ModuleInput::package(
            ModuleId(2),
            "lodash_map",
            "node_modules/lodash/map.js",
            "lodash",
            Some("4.17.21".to_string()),
        ));
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(1),
            target: ModuleDependencyTarget::Package {
                specifier: "lodash/map".to_string(),
            },
        });
        rows.package_attributions.push(
            PackageAttributionInput::accepted_external(
                ModuleId(2),
                "lodash",
                "4.17.21",
                "lodash/map",
            )
            .with_subpath("map"),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.is_clean());
        assert!(matches!(
            output.program.package_imports()[0].resolution,
            PackageResolution::External { .. }
        ));
    }

    #[test]
    fn accepted_project_package_surface_resolves_source_backed_external_import() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("const client = require('undici'); export { client };".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        rows.package_surfaces
            .push(PackageSurfaceInput::accepted_external(
                "undici", "2.2.1", "undici",
            ));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.is_clean());
        assert_eq!(output.program.package_imports().len(), 1);
        assert!(output.program.package_imports()[0].source_backed);
        assert!(matches!(
            output.program.package_imports()[0].resolution,
            PackageResolution::External { .. }
        ));
    }

    #[test]
    fn semantic_naming_sanitizes_reserved_words() {
        let mut rows = valid_rows();
        rows.symbols.push(SymbolInput::new(ModuleId(1), "class"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));
        let binding = output
            .program
            .semantic_names()
            .binding_name(ModuleId(1), "class")
            .expect("semantic binding should exist");

        assert_eq!(binding.as_str(), "_class");
    }

    #[test]
    fn semantic_name_hint_does_not_replace_source_binding_identity() {
        let mut rows = valid_rows();
        rows.symbols.push(
            SymbolInput::new(ModuleId(1), "$F1").with_semantic_name("lodashGlobalObjectInit"),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));
        let binding = output
            .program
            .semantic_names()
            .binding_name(ModuleId(1), "$F1")
            .expect("semantic binding should be keyed by original identity");

        assert_eq!(binding.as_str(), "lodashGlobalObjectInit");
        assert!(
            output
                .program
                .semantic_names()
                .binding_name(ModuleId(1), "lodashGlobalObjectInit")
                .is_none()
        );
    }

    #[test]
    fn unknown_package_surface_reports_unresolvable_import() {
        let mut rows = valid_rows();
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(1),
            target: ModuleDependencyTarget::Package {
                specifier: "lodash/map".to_string(),
            },
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.has(FindingCode::UnresolvableBareImport));
        assert!(matches!(
            output.program.package_imports()[0].resolution,
            PackageResolution::Rejected { .. }
        ));
    }

    #[test]
    fn ast_bare_import_uses_package_surface_resolution_without_duplicate_dependency_rows() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("import { map } from 'lodash/map'; export const answer = map;".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        rows.modules.push(ModuleInput::package(
            ModuleId(2),
            "lodash_map",
            "node_modules/lodash/map.js",
            "lodash",
            Some("4.17.21".to_string()),
        ));
        rows.package_attributions.push(
            PackageAttributionInput::accepted_external(
                ModuleId(2),
                "lodash",
                "4.17.21",
                "lodash/map",
            )
            .with_subpath("map"),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.is_clean());
        assert_eq!(output.program.package_imports().len(), 1);
        assert!(output.program.package_imports()[0].source_backed);
    }

    #[test]
    fn ast_bare_import_without_surface_reports_unresolvable_import() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("import { map } from 'lodash/map'; export const answer = map;".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.has(FindingCode::UnresolvableBareImport));
        assert_eq!(output.program.package_imports().len(), 1);
        assert!(output.program.package_imports()[0].source_backed);
    }

    #[test]
    fn ast_bare_reexport_uses_package_surface_resolution() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("export { map as lodashMap } from 'lodash/map';".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        rows.modules.push(ModuleInput::package(
            ModuleId(2),
            "lodash_map",
            "node_modules/lodash/map.js",
            "lodash",
            Some("4.17.21".to_string()),
        ));
        rows.package_attributions.push(
            PackageAttributionInput::accepted_external(
                ModuleId(2),
                "lodash",
                "4.17.21",
                "lodash/map",
            )
            .with_subpath("map"),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.is_clean());
        assert_eq!(output.program.package_imports().len(), 1);
        assert!(output.program.package_imports()[0].source_backed);
        assert!(matches!(
            output.program.package_imports()[0].resolution,
            PackageResolution::External { .. }
        ));
    }

    #[test]
    fn ast_fact_extraction_failure_is_reported_as_audit_finding() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "broken.js",
            Some("const =".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.has(FindingCode::AstFactExtractionFailed));
    }

    #[test]
    fn compiler_profile_detector_classifies_runtime_fingerprints() {
        let no_wrappers = BTreeSet::new();
        assert_eq!(
            detect_module_compiler_profile("", &identifiers(["__webpack_require__"]), &no_wrappers)
                .compiler,
            CompilerKind::Webpack
        );
        assert_eq!(
            detect_module_compiler_profile("", &identifiers(["__toCommonJS"]), &no_wrappers)
                .compiler,
            CompilerKind::Esbuild
        );
        assert_eq!(
            detect_module_compiler_profile(
                "Object.freeze({ answer: 42 })",
                &BTreeSet::new(),
                &no_wrappers
            )
            .compiler,
            CompilerKind::Rollup
        );
        assert_eq!(
            detect_module_compiler_profile(
                "",
                &identifiers(["_interopRequireDefault"]),
                &no_wrappers
            )
            .compiler,
            CompilerKind::Babel
        );
        assert_eq!(
            detect_module_compiler_profile(
                "function a(b){return b?b.c?b.c.d:b.c:b}var c={};for(var d=0;d<200;d++)c[d]=a({c:{d:d}});module.exports=c;function e(f){return f&&f.g?f.g.h:0}exports.e=e;",
                &BTreeSet::new(),
                &no_wrappers
            )
            .compiler,
            CompilerKind::Terser
        );
    }

    #[test]
    fn enrich_program_records_compiler_profile_from_ast_facts() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some("__webpack_require__(1);".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert_eq!(
            output
                .program
                .compiler_profile()
                .module(ModuleId(1))
                .compiler,
            CompilerKind::Webpack
        );
    }

    #[test]
    fn top_level_iife_wrappers_are_recorded_as_compiler_evidence() {
        use reverts_graph::AstWrapperKind;
        use reverts_model::CompilerEvidence;

        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some("(function(){ var x = 1; })();\n(()=>{ var y = 2; })();\n".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        let evidence = &output
            .program
            .compiler_profile()
            .module(ModuleId(1))
            .evidence;
        assert!(
            evidence.contains(&CompilerEvidence::TopLevelIife(
                AstWrapperKind::FunctionIife
            )),
            "expected FunctionIife evidence, got: {evidence:?}",
        );
        assert!(
            evidence.contains(&CompilerEvidence::TopLevelIife(AstWrapperKind::ArrowIife)),
            "expected ArrowIife evidence, got: {evidence:?}",
        );
    }

    #[test]
    fn incompatible_ast_shape_facts_are_reported_as_audit_finding() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "enum-conflict.js",
            Some(
                r#"
var NativeModuleType;
(function (NativeModuleType) {
    NativeModuleType[NativeModuleType["File"] = 0] = "File";
})(NativeModuleType || (NativeModuleType = {}));
NativeModuleType();
"#
                .to_string(),
            ),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.has(FindingCode::AmbiguousBindingShape));
    }

    #[test]
    fn object_literal_called_as_function_reports_callable_shape_failure() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "callable-conflict.js",
            Some("const factory = {}; factory();".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.has(FindingCode::CallableEmittedAsNonCallable));
    }

    #[test]
    fn unresolved_ast_read_is_reported_as_missing_definition() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "missing-read.js",
            Some("missing();".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.has(FindingCode::MissingDefinition));
        assert!(output.audit.findings().iter().any(|finding| {
            finding.code == FindingCode::MissingDefinition
                && finding.binding.as_deref() == Some("missing")
        }));
    }

    #[test]
    fn unresolved_ast_write_is_reported_as_missing_definition() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "missing-write.js",
            Some("missing = 1;".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.findings().iter().any(|finding| {
            finding.code == FindingCode::MissingDefinition
                && finding.binding.as_deref() == Some("missing")
                && finding.message.contains("written")
        }));
    }

    #[test]
    fn duplicate_top_level_ast_definition_is_reported() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "duplicate.js",
            Some("var value = 1; var value = 2;".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.has(FindingCode::DuplicateTopLevelBinding));
        assert!(output.audit.findings().iter().any(|finding| {
            finding.code == FindingCode::DuplicateTopLevelBinding
                && finding.binding.as_deref() == Some("value")
        }));
    }

    #[test]
    fn ambient_runtime_globals_do_not_fail_def_use_audit() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "runtime.js",
            Some("console.log(process.cwd());".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(!output.audit.has(FindingCode::MissingDefinition));
    }

    #[test]
    fn ambient_browser_and_extension_globals_do_not_fail_def_use_audit() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "browser-runtime.js",
            Some(
                r#"
                chrome.runtime.sendMessage({ ok: true });
                fetch(new Request("/api"), { headers: new Headers() });
                const blob = new Blob(["ok"]);
                new DOMParser().parseFromString("<p>ok</p>", "text/html");
                localStorage.setItem("blob", URL.createObjectURL(blob));
                "#
                .to_string(),
            ),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(!output.audit.has(FindingCode::MissingDefinition));
    }

    fn identifiers<const N: usize>(values: [&str; N]) -> BTreeSet<String> {
        values.into_iter().map(str::to_string).collect()
    }
}
