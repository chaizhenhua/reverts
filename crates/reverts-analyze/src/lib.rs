use std::collections::{BTreeMap, BTreeSet};

use reverts_graph::{AstFactKind, AstWrapperKind, FunctionExtractor};
use reverts_input::{ModuleDependencyTarget, SymbolScope};
use reverts_ir::{
    BindingConstraintKind, BindingName, BindingShapeSolution, ControlFlowEdgeKind,
    ControlFlowNodeKind, FunctionFingerprint, ModuleId,
};
use reverts_js::sanitize_identifier;
pub use reverts_model::CompilerKind;
use reverts_model::{
    CompilerEvidence, CompilerProfile, EnrichedProgram, ModuleCompilerProfile,
    PackageImportDecision, ProgramModel, SemanticNameMap,
};
use reverts_observe::{AuditFinding, AuditReport, FindingCode};
use reverts_package::{PackageResolution, PackageSurfaceIndex};

#[derive(Debug, Clone, PartialEq)]
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
    audit.extend(audit_unprotected_nullable_member_reads(&model));
    audit.extend(audit_unreachable_top_level_code(&model));
    let package_imports = resolve_package_imports(&model, &package_index, &mut audit);

    let mut function_fingerprints: BTreeMap<ModuleId, Vec<FunctionFingerprint>> = BTreeMap::new();
    if should_collect_enrichment_function_fingerprints(&model) {
        for module in model.modules() {
            if let Some(slice) = model.input().module_source_slice(module.id) {
                let fps = FunctionExtractor::fingerprint(module.id, slice.source);
                if !fps.is_empty() {
                    function_fingerprints.insert(module.id, fps);
                }
            }
        }
    }

    EnrichmentOutput {
        program: EnrichedProgram::new(model, semantic_names, package_imports, binding_shapes)
            .with_compiler_profile(compiler_profile)
            .with_function_fingerprints(function_fingerprints),
        audit,
    }
}

const ENRICHMENT_FUNCTION_FINGERPRINT_MODULE_LIMIT: usize = 1024;

fn should_collect_enrichment_function_fingerprints(model: &ProgramModel) -> bool {
    model.modules().len() <= ENRICHMENT_FUNCTION_FINGERPRINT_MODULE_LIMIT
        || std::env::var_os("REVERTS_COLLECT_FUNCTION_FINGERPRINTS").is_some()
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
        // The input bundle has multiple top-level declarations of the same
        // name. JS permits this for `var`/`function`; for `let`/`const` it's
        // a parse error in the source, but bundlers regularly emit code
        // where two distinct logical bindings happen to share a generated
        // name. Per ADR 0002 the decompiler is faithful, not corrective:
        // we surface the duplicate so consumers can disambiguate, but we
        // don't strand emission on this module-local condition.
        audit.push(
            AuditFinding::warning(
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
            | "eval"
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

/// Walk the lightweight CFG and flag statements that follow a top-level
/// `return` or `throw` via a Sequential edge — those are unreachable. Skips
/// the implicit Exit node so the final terminator does not falsely fire.
fn audit_unreachable_top_level_code(model: &ProgramModel) -> AuditReport {
    let mut audit = AuditReport::default();
    let cfg = model.graph().control_flow();
    for module in model.modules() {
        let nodes = cfg.nodes_for(module.id);
        if nodes.is_empty() {
            continue;
        }
        for edge in cfg.edges_for(module.id) {
            if edge.kind != ControlFlowEdgeKind::Sequential {
                continue;
            }
            let Some(from_node) = nodes.iter().find(|node| node.id == edge.from) else {
                continue;
            };
            if !matches!(
                from_node.kind,
                ControlFlowNodeKind::Return | ControlFlowNodeKind::Throw
            ) {
                continue;
            }
            let Some(to_node) = nodes.iter().find(|node| node.id == edge.to) else {
                continue;
            };
            if to_node.kind == ControlFlowNodeKind::Exit {
                continue;
            }
            audit.push(
                AuditFinding::error(
                    FindingCode::UnreachableTopLevelCode,
                    "module body contains a statement that follows a top-level return or throw",
                )
                .with_module(module.id.0.to_string()),
            );
        }
    }
    audit
}

fn audit_binding_shape_conflicts(binding_shapes: &BindingShapeSolution) -> AuditReport {
    let mut audit = AuditReport::default();
    for conflict in binding_shapes.conflicts() {
        let code = binding_shape_conflict_code(conflict.existing_kind, conflict.incoming_kind);
        let message = format!(
            "binding has incompatible shape constraints: {:?} requires {:?}, {:?} requires {:?}",
            conflict.existing_kind,
            conflict.existing_shape,
            conflict.incoming_kind,
            conflict.incoming_shape
        );
        // `CallableEmittedAsNonCallable` is a structural mistake the
        // emitter can introduce (object literal where a callable was
        // required), so it stays an error: emission must abort.
        // `AmbiguousBindingShape` indicates the *input bundle* uses the
        // same name with conflicting shapes; faithful-not-corrective per
        // ADR 0002 keeps emission going with a warning.
        let finding = match code {
            FindingCode::CallableEmittedAsNonCallable => AuditFinding::error(code, message),
            _ => AuditFinding::warning(code, message),
        };
        audit.push(
            finding
                .with_module(conflict.module_id.0.to_string())
                .with_binding(conflict.binding.as_str().to_string()),
        );
    }
    audit
}

/// Surface the input-bundle pattern `X = (await fetch(...)).data.value;`
/// followed by an unguarded member-read on a binding that aliases `X`.
/// ADR 0002 forbids repairing the input — we warn so the user knows the
/// original bundle has a latent null deref, while keeping the emit
/// faithful.
///
/// The alias closure resolves direct accesses (`X.foo`) and any
/// module-scope alias chain (`A = X; A.foo`, `A = getX(); A.foo` where
/// `getX()` returns X and the call result is bound at module scope).
/// Indirect access via function-local aliases (e.g. `let A` inside a
/// function) is currently NOT caught — locals are filtered out of the
/// fact extractor to avoid name collisions across nested functions, and
/// proper scope-qualified binding identity is a follow-up.
fn audit_unprotected_nullable_member_reads(model: &ProgramModel) -> AuditReport {
    use std::collections::BTreeSet;
    let mut audit = AuditReport::default();
    let def_use = model.graph().def_use();
    let maybe_nullable = def_use.maybe_nullable_writes();
    if maybe_nullable.is_empty() {
        return audit;
    }
    let mut read_targets: BTreeSet<(reverts_ir::ModuleId, reverts_ir::BindingName)> =
        BTreeSet::new();
    for constraint in def_use.constraints() {
        if matches!(
            constraint.kind,
            BindingConstraintKind::MemberRead | BindingConstraintKind::MemberWrite
        ) {
            read_targets.insert((constraint.module_id, constraint.binding.clone()));
        }
    }
    // Build the reverse map: for each maybe-nullable source binding, find
    // every binding that aliases (directly or transitively) back to it.
    // The audit then fires when any of those aliases is member-read.
    let mut already_reported: BTreeSet<(reverts_ir::ModuleId, reverts_ir::BindingName)> =
        BTreeSet::new();
    for (module_id, read_target) in &read_targets {
        let aliases = def_use.alias_sources_of(*module_id, read_target.as_str());
        for alias in &aliases {
            let key = (*module_id, alias.clone());
            if !maybe_nullable.contains(&key) || already_reported.contains(&key) {
                continue;
            }
            already_reported.insert(key);
            audit.push(
                // Documented as a warning per ADR 0002: the decompiler is
                // faithful, not corrective. Crash potential exists in the
                // original bundle; we surface it but don't strand emission.
                AuditFinding::warning(
                    FindingCode::UnprotectedNullableMemberRead,
                    format!(
                        "binding '{}' is assigned from a member chain on a call/await result and later member-read without a null guard — the original bundle can crash here",
                        alias.as_str()
                    ),
                )
                .with_module(module_id.0.to_string())
                .with_binding(alias.as_str().to_string()),
            );
        }
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
    let mut direct_names = BTreeMap::<(ModuleId, BindingName), String>::new();

    for module in model.modules() {
        semantic_names.insert_module_path(module.id, module.semantic_path.clone());
    }

    for symbol in model.symbols() {
        if symbol.scope != SymbolScope::Module {
            continue;
        }
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
        direct_names.insert(
            (symbol.module_id, BindingName::new(symbol.name.clone())),
            semantic_names
                .binding_name(symbol.module_id, symbol.name.as_str())
                .expect("semantic name was just inserted")
                .as_str()
                .to_string(),
        );
    }

    // Alias-derived names are lower-priority readability hints. They are
    // deliberately added after direct symbol/export names and get a suffix so
    // `source` and `alias` do not collapse into the same emitted identifier.
    for module in model.modules() {
        for original in model.graph().definitions_for(module.id) {
            if mapped_originals.contains(&(module.id, original.as_str().to_string())) {
                continue;
            }
            let aliases = model
                .graph()
                .def_use()
                .alias_sources_of(module.id, original.as_str());
            let Some(source_name) = aliases
                .iter()
                .filter(|alias| alias.as_str() != original.as_str())
                .find_map(|alias| direct_names.get(&(module.id, alias.clone())))
            else {
                continue;
            };
            let base = sanitize_identifier(alias_semantic_name(source_name).as_str());
            let semantic = reserve_unique_name(&mut used_by_module, module.id, &base);
            semantic_names.insert_binding(module.id, original.as_str(), semantic);
            mapped_originals.insert((module.id, original.as_str().to_string()));
        }
    }

    semantic_names
}

fn alias_semantic_name(source_name: &str) -> String {
    if source_name.ends_with("Alias") {
        source_name.to_string()
    } else {
        format!("{source_name}Alias")
    }
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

    let compiler = if collect_runtime_identifier_evidence(
        identifiers,
        WEBPACK_RUNTIME_IDENTIFIERS,
        &mut evidence,
    ) {
        CompilerKind::Webpack
    } else if collect_runtime_identifier_evidence(
        identifiers,
        ESBUILD_RUNTIME_IDENTIFIERS,
        &mut evidence,
    ) {
        CompilerKind::Esbuild
    } else if collect_runtime_identifier_evidence(
        identifiers,
        ROLLUP_RUNTIME_IDENTIFIERS,
        &mut evidence,
    ) {
        CompilerKind::Rollup
    } else if collect_runtime_identifier_evidence(
        identifiers,
        BABEL_RUNTIME_IDENTIFIERS,
        &mut evidence,
    ) {
        CompilerKind::Babel
    } else if minified {
        CompilerKind::Terser
    } else {
        CompilerKind::Unknown
    };

    ModuleCompilerProfile::new(compiler, minified, evidence)
}

/// Identifier-based detection. Runtime helper names are accepted only when the
/// AST fact extractor reports them as identifier evidence.
fn collect_runtime_identifier_evidence(
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

pub const WEBPACK_RUNTIME_IDENTIFIERS: &[&str] = &[
    "__webpack_require__",
    "__webpack_exports__",
    "__webpack_modules__",
    "__webpack_module_cache__",
    "webpackChunk",
    "webpackJsonp",
];

pub const ESBUILD_RUNTIME_IDENTIFIERS: &[&str] = &[
    "__defProp",
    "__export",
    "__copyProps",
    "__toESM",
    "__toCommonJS",
    "__commonJS",
    "__require",
];

pub const ROLLUP_RUNTIME_IDENTIFIERS: &[&str] = &[
    "commonjsGlobal",
    "getDefaultExportFromCjs",
    "getAugmentedNamespace",
    "_mergeNamespaces",
    "_interopNamespaceDefault",
    "_interopDefaultLegacy",
];

pub const BABEL_RUNTIME_IDENTIFIERS: &[&str] = &[
    "_interopRequireDefault",
    "_interopRequireWildcard",
    "_classCallCheck",
    "_createClass",
    "_defineProperty",
    "_inherits",
    "_possibleConstructorReturn",
    "regeneratorRuntime",
];

/// esbuild output wrapper function names. These are emitted by the esbuild
/// runtime around imported CJS modules, exported namespaces, and helper
/// inits; `reverts-js::normalize::BundlerWrapperUnwrapped` strips them for
/// `ast_hash` collision, `reverts-bundle::detectors::esbuild` recognises
/// them as module boundaries.
///
/// The definition lives in
/// [`reverts_js::normalize::bundler_wrapper_unwrapped::ESBUILD_WRAPPER_NAMES`]
/// and is re-exported here as the single stable public surface for callers
/// outside `reverts-js`.
pub use reverts_js::normalize::bundler_wrapper_unwrapped::ESBUILD_WRAPPER_NAMES;

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

    fn rows_with_application_source(source: &str) -> InputRows {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some(source.to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
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
    fn semantic_naming_propagates_direct_hint_to_aliases() {
        let mut rows = rows_with_application_source("var a = 1; var b = a; console.log(a, b);");
        rows.symbols
            .push(SymbolInput::new(ModuleId(1), "a").with_semantic_name("settings"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));
        let names = output.program.semantic_names();

        assert_eq!(
            names
                .binding_name(ModuleId(1), "a")
                .expect("direct semantic name should exist")
                .as_str(),
            "settings"
        );
        assert_eq!(
            names
                .binding_name(ModuleId(1), "b")
                .expect("alias semantic name should exist")
                .as_str(),
            "settingsAlias"
        );
    }

    #[test]
    fn semantic_naming_direct_alias_hint_wins_over_propagated_hint() {
        let mut rows = rows_with_application_source("var a = 1; var b = a; console.log(a, b);");
        rows.symbols
            .push(SymbolInput::new(ModuleId(1), "a").with_semantic_name("settings"));
        rows.symbols
            .push(SymbolInput::new(ModuleId(1), "b").with_semantic_name("preferredAlias"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));
        let names = output.program.semantic_names();

        assert_eq!(
            names
                .binding_name(ModuleId(1), "b")
                .expect("direct semantic name should exist")
                .as_str(),
            "preferredAlias"
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
            detect_module_compiler_profile("", &identifiers(["commonjsGlobal"]), &no_wrappers)
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
    fn top_level_arrow_iife_alone_does_not_classify_as_esbuild() {
        // A wrapper shape alone is not enough to infer the compiler. Without
        // identifier or explicit pattern evidence, classification stays Unknown.
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some("(()=>{ var x = 1; x.foo = 2; })();\n".to_string()),
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
            CompilerKind::Unknown,
        );
    }

    #[test]
    fn babel_es_module_marker_alone_stays_unknown() {
        // The `Object.defineProperty(exports, "__esModule", ...)` marker is a
        // common CJS artifact and is not enough evidence without a Babel
        // helper identifier extracted from the AST facts.
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some(
                "\"use strict\";\nObject.defineProperty(exports, \"__esModule\", { value: true });\nexports.foo = 1;\n"
                    .to_string(),
            ),
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
            CompilerKind::Unknown,
        );
    }

    #[test]
    fn babel_jsx_runtime_import_alone_stays_unknown() {
        // JSX runtime import specifiers are ordinary dependency edges. They
        // do not prove Babel without a Babel helper identifier extracted from
        // the AST facts.
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some(
                "import { jsx as _jsx } from \"react/jsx-runtime\";\nvar x = _jsx(\"div\", {});\n"
                    .to_string(),
            ),
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
            CompilerKind::Unknown,
        );
    }

    #[test]
    fn top_level_throw_followed_by_statement_is_reported_as_unreachable() {
        // CommonJS scripts may have a top-level `return` and ESM modules may
        // have a top-level `throw`; either way, code that follows them
        // unconditionally cannot run and the CFG-based audit must surface that.
        let source = "throw new Error(\"boom\");\nvar leftover = 1;\n";
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some(source.to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(
            output.audit.has(FindingCode::UnreachableTopLevelCode),
            "expected UnreachableTopLevelCode finding, got: {:?}",
            output.audit.findings(),
        );
    }

    #[test]
    fn final_top_level_throw_alone_is_not_reported_as_unreachable() {
        // A single `throw` at the end of a module is reachable; the audit must
        // not fire on it.
        let source = "var x = 1;\nthrow new Error(\"boom\");\n";
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some(source.to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(
            !output.audit.has(FindingCode::UnreachableTopLevelCode),
            "audit must not fire on a final throw, got: {:?}",
            output.audit.findings(),
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

    #[test]
    fn esbuild_wrapper_names_list_covers_known_wrappers() {
        let names: std::collections::BTreeSet<&'static str> =
            super::ESBUILD_WRAPPER_NAMES.iter().copied().collect();
        for name in [
            "__toESM",
            "__toCommonJS",
            "__commonJS",
            "__esm",
            "__defProp",
            "__defProps",
            "__export",
            "__exportStar",
            "__reExport",
            "__copyProps",
        ] {
            assert!(names.contains(name), "missing wrapper name {name}");
        }
    }

    #[test]
    fn enriched_program_contains_function_fingerprints_for_modules_with_source() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("function add(a, b) { return a + b; }".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        let fps = output
            .program
            .function_fingerprints()
            .get(&ModuleId(1))
            .expect("module 1 should have fingerprints");
        assert!(
            !fps.is_empty(),
            "expected at least one function fingerprint"
        );
        assert!(
            fps.iter().any(|fp| fp.param_count == 2),
            "expected the 2-param `add` fn to be fingerprinted",
        );
    }
}
