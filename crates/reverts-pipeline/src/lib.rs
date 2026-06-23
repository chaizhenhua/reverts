mod assets;
mod audit;
mod output_paths;
mod pre_accept;
mod runtime_dependencies;
mod source_mirror;
mod source_rewrites;

use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::fmt;

use assets::{audit_required_assets, collect_emitted_assets};
pub use assets::{collect_required_asset_references, collect_required_asset_references_from_rows};
use audit::{
    audit_binding_shape_consistency, audit_emit_plan_synthesis,
    audit_emitted_named_export_consistency, audit_emitted_project_parse,
    audit_emitted_relative_import_targets, audit_module_file_sizes,
    audit_namespace_object_member_consistency, audit_required_sources,
};
use output_paths::module_output_paths;
pub(crate) use output_paths::relative_asset_specifier;
use runtime_dependencies::collect_runtime_dependencies;
use source_mirror::collect_source_mirror_assets;
pub(crate) use source_rewrites::rewrite_string_literal_values;

use reverts_analyze::enrich_program;
use reverts_emitter::{EmitError, emit_validated_project};
use reverts_input::{InputBundle, InputBundleError, InputRows, ModuleInput, SourceFileInput};
use reverts_ir::{BindingName, ModuleId};
use reverts_model::{EnrichedProgram, ProgramModel};
use reverts_observe::AuditReport;
use reverts_planner::{ImportExportPlanner, PlanError};

pub use pre_accept::{AcceptedProject, PreAcceptProject, PreAcceptTransformReport};
pub use reverts_emitter::{EmittedFile, EmittedProject};
pub use reverts_planner::{
    IslandClusterRecord, RuntimeSetterMigrationBindingKey, RuntimeSetterMigrationBindingStatus,
    RuntimeSetterMigrationBlockerReason, RuntimeSetterMigrationBlockerReport,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputRun {
    pub project: PreAcceptProject,
    pub accepted_project: Option<AcceptedProject>,
    pub pre_accept_report: Option<PreAcceptTransformReport>,
    pub audit: AuditReport,
    pub runtime_dependencies: Vec<RuntimeDependency>,
    pub assets: Vec<EmittedAsset>,
    pub source_mirror_assets: Vec<EmittedAsset>,
    /// Owning module for every emitted module file. Unlike `symbol_index`, this
    /// includes symbol-less modules (string/data modules, side-effect modules,
    /// tiny re-export wrappers), so downstream matchers can still compare their
    /// emitted source against package or first-party reference sources.
    pub module_output_paths: BTreeMap<ModuleId, String>,
    /// Maps each emitted module-level binding to its original (DB) name and the
    /// file it lands in, so a downstream naming agent can read the generated
    /// TypeScript and route names back to the right `(module_id, original)`.
    pub symbol_index: Vec<SymbolIndexEntry>,
    /// Emitted paths of planner-marked unmodularized recovered-code files
    /// (e.g. the eager entrypoint island): first-party application code owned
    /// by no model module. Consumers must use this set — never path
    /// comparisons — to recognize such files.
    pub unmodularized_code_paths: std::collections::BTreeSet<String>,
    /// Fingerprint → emitted path for every island cluster file, so the generate
    /// command can publish the `cluster-names` worklist manifest.
    pub island_clusters: Vec<reverts_planner::IslandClusterRecord>,
}

/// One emitted module-level binding: where it appears and under which name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolIndexEntry {
    /// Owning model module, or `None` for a binding declared in an
    /// unmodularized recovered-code file (e.g. the eager entrypoint island a
    /// scope-hoisting bundler leaves around the entry). Module-less bindings
    /// are still first-party naming work; they are renamed through the
    /// file-path-keyed binding-name channel instead of the symbols table.
    pub module_id: Option<ModuleId>,
    /// Binding name as it exists in the graph / DB key space.
    pub original_name: String,
    /// Name as actually emitted (semantic name if assigned, else original).
    pub emitted_name: String,
    /// True only when the emitted name came from an accepted
    /// `symbols.semantic_name` entry or an accepted file-level binding rename
    /// (`semantic_binding_names`). Meaningful-looking preserved names do not
    /// count as semantic naming evidence.
    pub semantic_named: bool,
    /// Emitted file the binding lands in.
    pub file_path: String,
    /// Whether the emitted declaration is a function or class (vs a value/const).
    pub function_like: bool,
    /// Whether the binding is dead in its emitted module: an unexported
    /// module-scope binding that is never read or written (an esbuild vestigial
    /// hoist or unused constant). Dead bindings carry no semantic role and are
    /// excluded from the naming worklist/denominator.
    pub dead: bool,
    /// Whether the emitted file exports this binding (parsed from the emitted
    /// source). Authoritative for module-less bindings, whose exports exist in
    /// no graph; informational alongside graph exports for module bindings.
    pub exported: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GenerateProjectOptions {
    pub local_binding_renames: Vec<LocalBindingRename>,
    pub function_param_renames: Vec<FunctionParamRenameRow>,
    /// Bundle-local names of eager entry-island bindings that the package
    /// matcher anchored to a third-party library (see `package_island_anchors`).
    /// These are library code flattened into the island, not application
    /// symbols, so they are dropped from the naming denominator — included here
    /// they would inflate it and depress coverage with work no one should do.
    pub package_anchored_island_bindings: std::collections::BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalBindingRename {
    pub file_path: String,
    pub original_name: String,
    pub binding_index: Option<u32>,
    pub semantic_name: String,
}

/// One recovered parameter name: rename the `param_index`-th formal parameter of
/// `function_name` in `file_path` to `semantic_name`. Keyed by function name and
/// position, applied by the emitter's function-param pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionParamRenameRow {
    pub file_path: String,
    pub function_name: String,
    pub param_index: u32,
    pub semantic_name: String,
}

/// Builds [`SymbolIndexEntry`] rows for the top-level bindings that actually
/// appear in each emitted file.
///
/// The universe is parsed from the *emitted* TypeScript, not the input graph:
/// reconstruction un-wraps bundle-scoped bindings (e.g. esbuild modules whose
/// `var U$, sG` sit inside a wrapper in the minified input) up to module top
/// level, so input-graph `definitions_for` would miss them. `original_name` is
/// recovered from the semantic-name overlay so already-renamed bindings still
/// map back to their `(module_id, original)` DB key; unnamed bindings key by
/// their own (still-minified) identifier.
///
/// Files owned by no module are scaffold/runtime glue and contribute nothing —
/// EXCEPT the planner-marked unmodularized recovered-code files
/// (`unmodularized_code_paths`, e.g. the eager entrypoint island): those hold
/// first-party application declarations that must enter the naming universe,
/// keyed by `module_id: None` + file path. Their accepted renames arrive
/// through `local_binding_renames` (the file-path-keyed binding-name channel),
/// which also supplies the renamed→original overlay for every file.
fn build_symbol_index(
    program: &EnrichedProgram,
    module_output_paths: &std::collections::BTreeMap<ModuleId, String>,
    emitted: &reverts_emitter::EmittedProject,
    unmodularized_code_paths: &std::collections::BTreeSet<String>,
    local_binding_renames: &[LocalBindingRename],
    package_anchored_island_bindings: &std::collections::BTreeSet<String>,
) -> Vec<SymbolIndexEntry> {
    use std::path::Path;

    use reverts_js::{
        ParseGoal, TopLevelStatementKind, collect_dead_top_level_bindings,
        collect_exported_top_level_bindings, collect_top_level_statement_facts,
    };

    // Emitted file path -> owning module. Merged modules collapse to the last
    // writer, which is acceptable for binding attribution.
    let mut module_for_path: std::collections::BTreeMap<&str, ModuleId> =
        std::collections::BTreeMap::new();
    for (module_id, path) in module_output_paths {
        module_for_path.insert(path.as_str(), *module_id);
    }

    // (module_id, emitted/semantic name) -> original DB name, for bindings with
    // an accepted semantic name, so index keys map back to the symbols table and
    // downstream coverage can distinguish explicit names from preserved text.
    // Unnamed emitted bindings still enter the worklist using their emitted
    // name as the original key; `symbol-names accept` can create missing symbol
    // rows from that generated plan, but they do not count as named until an
    // explicit semantic name is accepted.
    let mut original_for_semantic_emitted: std::collections::BTreeMap<(ModuleId, &str), &str> =
        std::collections::BTreeMap::new();
    let mut accepted_source_symbols: std::collections::BTreeSet<(ModuleId, &str)> =
        std::collections::BTreeSet::new();
    for symbol in program.model().symbols() {
        if let Some(semantic) = program
            .semantic_names()
            .binding_name(symbol.module_id, symbol.name.as_str())
        {
            original_for_semantic_emitted
                .insert((symbol.module_id, semantic.as_str()), symbol.name.as_str());
            accepted_source_symbols.insert((symbol.module_id, symbol.name.as_str()));
        }
    }

    // (file path, semantic emitted name) -> original name, from accepted
    // file-level binding renames. Those rewrite the emitted text directly, so
    // the index must map the renamed binding back to its original key and count
    // it as semantically named — for module files and module-less files alike.
    // Index-scoped renames target inner (non-top-level) bindings and are
    // excluded: matching them by name alone could mislabel an unrelated
    // top-level binding.
    let mut original_for_renamed: std::collections::BTreeMap<(&str, &str), &str> =
        std::collections::BTreeMap::new();
    for rename in local_binding_renames {
        if rename.binding_index.is_none() {
            original_for_renamed.insert(
                (rename.file_path.as_str(), rename.semantic_name.as_str()),
                rename.original_name.as_str(),
            );
        }
    }

    let mut entries = Vec::new();
    for file in &emitted.files {
        let module_id = module_for_path.get(file.path.as_str()).copied();
        if module_id.is_none() && !unmodularized_code_paths.contains(file.path.as_str()) {
            continue; // scaffold / runtime files with no owning module
        }
        let Ok(facts) = collect_top_level_statement_facts(
            &file.source,
            Some(Path::new(&file.path)),
            ParseGoal::TypeScript,
        ) else {
            continue;
        };
        // Dead (unexported, unreferenced) module-scope bindings — esbuild
        // vestigial hoists and unused constants — carry no semantic role and are
        // excluded from the naming worklist downstream.
        let dead_bindings = collect_dead_top_level_bindings(
            &file.source,
            Some(Path::new(&file.path)),
            ParseGoal::TypeScript,
        )
        .unwrap_or_default();
        let exported_bindings = collect_exported_top_level_bindings(
            &file.source,
            Some(Path::new(&file.path)),
            ParseGoal::TypeScript,
        )
        .unwrap_or_default();
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for fact in facts {
            if !matches!(
                fact.kind,
                TopLevelStatementKind::Function
                    | TopLevelStatementKind::Class
                    | TopLevelStatementKind::Variable
                    | TopLevelStatementKind::LazyValue
                    | TopLevelStatementKind::LazyModule
                    | TopLevelStatementKind::Export
            ) {
                continue;
            }
            let function_like = matches!(
                fact.kind,
                TopLevelStatementKind::Function | TopLevelStatementKind::Class
            );
            for emitted_name in fact.bindings {
                if !seen.insert(emitted_name.clone()) {
                    continue;
                }
                let semantic_original = module_id.and_then(|module_id| {
                    original_for_semantic_emitted
                        .get(&(module_id, emitted_name.as_str()))
                        .copied()
                });
                let renamed_original = original_for_renamed
                    .get(&(file.path.as_str(), emitted_name.as_str()))
                    .copied();
                let (original_name, semantic_named) =
                    if let Some(semantic_original) = semantic_original {
                        (semantic_original.to_string(), true)
                    } else if let Some(renamed_original) = renamed_original {
                        (renamed_original.to_string(), true)
                    } else if module_id.is_some_and(|module_id| {
                        accepted_source_symbols.contains(&(module_id, emitted_name.as_str()))
                    }) {
                        continue;
                    } else {
                        (emitted_name.clone(), false)
                    };
                // Library code the matcher anchored to a package was flattened
                // into the island; it is not application naming work, so it
                // leaves the denominator entirely. Gated on `module_id.is_none()`
                // so a same-named binding in a real module is never dropped.
                if module_id.is_none() && package_anchored_island_bindings.contains(&original_name)
                {
                    continue;
                }
                let dead = dead_bindings.contains(&emitted_name);
                let exported = exported_bindings.contains(&emitted_name);
                entries.push(SymbolIndexEntry {
                    module_id,
                    original_name,
                    emitted_name,
                    semantic_named,
                    file_path: file.path.clone(),
                    function_like,
                    dead,
                    exported,
                });
            }
        }
    }
    entries
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeDependency {
    pub package_name: String,
    pub package_version: String,
}

/// Declare each island-externalized package as a runtime dependency, so the
/// output `package.json` lists the package the recovered island now imports
/// (`import * as … from '<pkg>'`) instead of inlining it. Skips specifiers
/// already present from module-level attributions.
fn extend_runtime_dependencies_with_island_externalizations(
    runtime_dependencies: &mut Vec<RuntimeDependency>,
    program: &EnrichedProgram,
) {
    for externalization in program.island_package_externalizations() {
        let already_listed = runtime_dependencies
            .iter()
            .any(|dependency| dependency.package_name == externalization.import_specifier);
        if already_listed {
            continue;
        }
        runtime_dependencies.push(RuntimeDependency {
            package_name: externalization.import_specifier.clone(),
            package_version: externalization.version.clone(),
        });
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmittedAsset {
    pub path: String,
    pub bytes: Vec<u8>,
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AssetReference {
    pub module_id: ModuleId,
    pub logical_path: String,
}

/// Result of bundle-aware row preparation for the matcher path. The
/// matcher needs `synthetic_modules` separately so it can persist them
/// into SQLite alongside `INSERT OR IGNORE` deduplication.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedInputRows {
    pub rows: InputRows,
    pub synthetic_modules: Vec<ModuleInput>,
    pub audit: AuditReport,
}

/// Enriched program plus the bundle-extraction audit that ran ahead of
/// it. Lets `generate_project_from_prepared` and
/// `runtime_setter_migration_blocker_report_from_prepared` share a single
/// enrichment when the same project needs both outputs (the CLI inventory
/// flow with `--setter-blockers`).
#[derive(Debug)]
pub struct PreparedProgram {
    pub program: EnrichedProgram,
    pub bundle_audit: AuditReport,
    pub enrichment_audit: AuditReport,
}

/// Bundle-aware row preparation for the matcher.
///
/// Always runs `reverts_bundle::extract`. At match time the loaded rows
/// have the bundle wrapper attached as a top-level module on the source
/// file, so the missing-modules filter used by the generator path would
/// incorrectly skip extraction here. Idempotency relies on
/// `reverts_bundle::extract`'s synthetic-id allocator starting past
/// `max_real_id`, so re-runs after persistence produce no new modules.
#[must_use]
pub fn prepare_input_rows_for_pipeline(rows: InputRows) -> PreparedInputRows {
    prepare_input_rows_for_pipeline_with_reserved_ids(rows, 0)
}

/// Like [`prepare_input_rows_for_pipeline`], but reserves `0..=reserved_max_id`
/// for the synthetic-id allocator. Matcher callers pass the WHOLE-table
/// `MAX(modules.id, modules.file_id)` so freshly reconstructed synthetic
/// sources cannot alias an orphan module's `file_id` (a legacy reconstruction
/// dropped from the load by the `project_files` filter) and resurrect it
/// against a mismatched source span on the next load.
#[must_use]
pub fn prepare_input_rows_for_pipeline_with_reserved_ids(
    mut rows: InputRows,
    reserved_max_id: u32,
) -> PreparedInputRows {
    let extraction = reverts_bundle::extract_with_reserved_ids(
        &rows.source_files,
        &rows.modules,
        reserved_max_id,
    );
    let synthetic_modules = extraction.new_modules.clone();
    let audit = extraction.audit.clone();
    extraction.merge_into(&mut rows);
    if let Ok(input) = InputBundle::from_rows(rows.clone()) {
        let model = ProgramModel::from_input(input);
        let enrichment = enrich_program(model);
        rows.dependencies = enrichment.program.model().input().dependencies.clone();
    }
    PreparedInputRows {
        rows,
        synthetic_modules,
        audit,
    }
}

/// Bundle-aware bundle preparation for the generator.
///
/// When called after the matcher has persisted synthetic modules and the
/// bundle has been reloaded, every bundle source file already has its
/// inner modules attached; in that case extraction is skipped. When
/// called standalone (no prior matcher run) on a bundle where no module
/// points at a source file, extraction runs on those source files only
/// and the resulting synthetic modules are appended via
/// `InputBundle::with_appended_modules` — no full row re-validation.
pub fn prepare_input_bundle_for_generation(
    input: InputBundle,
) -> Result<(InputBundle, AuditReport), PipelineError> {
    let candidates = missing_module_source_file_ids(&input);
    if candidates.is_empty() {
        return Ok((input, AuditReport::default()));
    }
    let candidate_source_files: Vec<&SourceFileInput> = input
        .source_files
        .iter()
        .filter(|sf| candidates.contains(&sf.id))
        .collect();
    // `reverts_bundle::extract` wants a slice; build a tiny owned vec of
    // refs by cloning only the candidate source files — modules and the
    // remaining unrelated source files are not touched.
    let candidate_source_files: Vec<SourceFileInput> =
        candidate_source_files.into_iter().cloned().collect();
    let extraction = reverts_bundle::extract(&candidate_source_files, &input.modules);
    let audit = extraction.audit.clone();
    let new_source_files = extraction.new_source_files;
    let new_modules = extraction.new_modules;
    // Synthetic source files (reconstructed esbuild multi-handle modules) must
    // be appended BEFORE their modules so the module → source_file FK resolves.
    let input = input
        .with_appended_source_files(new_source_files)
        .map_err(PipelineError::Input)?
        .with_appended_modules(new_modules)
        .map_err(PipelineError::Input)?;
    Ok((input, audit))
}

pub fn runtime_setter_migration_blocker_report_from_input(
    input: InputBundle,
) -> Result<RuntimeSetterMigrationBlockerReport, PipelineError> {
    let prepared = prepare_and_enrich(input)?;
    Ok(runtime_setter_migration_blocker_report_from_prepared(
        &prepared,
    ))
}

#[must_use]
pub fn runtime_setter_migration_blocker_report_from_prepared(
    prepared: &PreparedProgram,
) -> RuntimeSetterMigrationBlockerReport {
    ImportExportPlanner.runtime_setter_migration_blocker_report(&prepared.program)
}

pub fn prepare_and_enrich(input: InputBundle) -> Result<PreparedProgram, PipelineError> {
    let (mut input, bundle_audit) = prepare_input_bundle_for_generation(input)?;
    // Merge exact-duplicate modules so a binding they define resolves to ONE
    // owner (not an ambiguous pair that drops the cross-module import).
    input.dedup_identical_modules();
    // Strip cross-bundle dependency leaks BEFORE the graph is built — the
    // ImportExport graph records module imports from these edges, so the planner
    // would otherwise resolve a binding to a foreign esbuild bundle (e.g. the
    // Node main process importing a renderer `ion-dist` chunk using `document`).
    input.strip_cross_bundle_dependencies();
    let model = ProgramModel::from_input(input);
    let enrichment = enrich_program(model);
    Ok(PreparedProgram {
        program: enrichment.program,
        bundle_audit,
        enrichment_audit: enrichment.audit,
    })
}

pub fn generate_project_from_input(input: InputBundle) -> Result<OutputRun, PipelineError> {
    let prepared = prepare_and_enrich(input)?;
    generate_project_from_prepared(prepared)
}

pub fn generate_project_from_input_with_options(
    input: InputBundle,
    options: GenerateProjectOptions,
) -> Result<OutputRun, PipelineError> {
    let timing_enabled = std::env::var_os("REVERTS_GENERATE_TIMING").is_some();
    let started = std::time::Instant::now();
    let prepared = prepare_and_enrich(input)?;
    if timing_enabled {
        eprintln!(
            "generate-project timing: prepare_and_enrich total={:.3}s",
            started.elapsed().as_secs_f64()
        );
    }
    generate_project_from_prepared_with_options(prepared, options)
}

pub fn generate_project_from_prepared(
    prepared: PreparedProgram,
) -> Result<OutputRun, PipelineError> {
    generate_project_from_prepared_with_options(prepared, GenerateProjectOptions::default())
}

pub fn generate_project_from_prepared_with_options(
    prepared: PreparedProgram,
    options: GenerateProjectOptions,
) -> Result<OutputRun, PipelineError> {
    let timing_enabled = std::env::var_os("REVERTS_GENERATE_TIMING").is_some();
    let started = std::time::Instant::now();
    let mut last = started;
    macro_rules! mark_timing {
        ($stage:literal) => {
            if timing_enabled {
                let now = std::time::Instant::now();
                eprintln!(
                    "generate-project timing: {} stage={:.3}s total={:.3}s",
                    $stage,
                    now.duration_since(last).as_secs_f64(),
                    now.duration_since(started).as_secs_f64()
                );
                last = now;
            }
        };
    }
    let PreparedProgram {
        program,
        bundle_audit,
        enrichment_audit,
    } = prepared;
    let mut audit = bundle_audit;
    audit.extend(enrichment_audit);
    let input = program.model().input();
    let mut runtime_dependencies = collect_runtime_dependencies(input);
    extend_runtime_dependencies_with_island_externalizations(&mut runtime_dependencies, &program);
    let asset_references = collect_required_asset_references(input);
    let assets = collect_emitted_assets(input, &asset_references);
    let source_mirror_assets = collect_source_mirror_assets(input);
    audit.extend(audit_required_sources(&program));
    audit.extend(audit_required_assets(input, &asset_references));
    mark_timing!("pre_plan_audits");
    // Only errors zero-out emission. Warnings (e.g. UnprotectedNullableMemberRead
    // by design per ADR 0002 — surfaced rather than repaired) must not strand
    // the entire project at files=0.
    if audit.has_errors() {
        return Ok(OutputRun {
            project: PreAcceptProject::empty(),
            accepted_project: None,
            pre_accept_report: None,
            audit,
            runtime_dependencies,
            assets,
            source_mirror_assets,
            module_output_paths: BTreeMap::new(),
            symbol_index: Vec::new(),
            unmodularized_code_paths: std::collections::BTreeSet::new(),
            island_clusters: Vec::new(),
        });
    }

    let planner = ImportExportPlanner;
    let mut plan = planner
        .plan_enriched_program(&program)
        .map_err(PipelineError::Plan)?;
    mark_timing!("plan");
    apply_local_binding_renames(&mut plan, &options.local_binding_renames);
    apply_function_param_renames_to_plan(&mut plan, &options.function_param_renames);
    audit.extend(audit_emit_plan_synthesis(&plan));
    mark_timing!("plan_audit");
    if audit.has_errors() {
        return Ok(OutputRun {
            project: PreAcceptProject::empty(),
            accepted_project: None,
            pre_accept_report: None,
            audit,
            runtime_dependencies,
            assets,
            source_mirror_assets,
            module_output_paths: BTreeMap::new(),
            symbol_index: Vec::new(),
            unmodularized_code_paths: std::collections::BTreeSet::new(),
            island_clusters: Vec::new(),
        });
    }

    let module_output_paths = module_output_paths(&program);
    let validated_plan = plan.clone().validate().map_err(PipelineError::Plan)?;
    mark_timing!("validate_plan");
    let outcome = emit_validated_project(&validated_plan).map_err(PipelineError::Emit)?;
    mark_timing!("emit");
    for finding in outcome.findings {
        audit.push(finding);
    }
    let pre_accept = pre_accept::apply_pre_accept_transforms(
        outcome.project,
        &pre_accept::PreAcceptContext {
            input,
            asset_references: &asset_references,
            module_output_paths: &module_output_paths,
        },
    );
    let emitted_project = pre_accept.project.clone();
    let pre_accept_report = pre_accept.report.clone();
    mark_timing!("pre_accept");

    // These four audits are independent, read-only scans of the immutable emitted
    // project/plan; run them concurrently (the parse audit alone re-parses every
    // emitted file). `thread::scope` borrows the shared inputs without `'static`
    // bounds; results are merged in a fixed order so findings stay deterministic.
    let (parse_findings, relative_findings, named_findings, shape_findings) =
        std::thread::scope(|scope| {
            let parse = scope.spawn(|| audit_emitted_project_parse(&emitted_project));
            let relative = scope.spawn(|| {
                audit_emitted_relative_import_targets(
                    &emitted_project,
                    &assets,
                    input,
                    &module_output_paths,
                )
            });
            let named = scope.spawn(|| audit_emitted_named_export_consistency(&emitted_project));
            let shape = scope.spawn(|| audit_binding_shape_consistency(&plan, &emitted_project));
            (
                parse.join().expect("parse audit thread"),
                relative.join().expect("relative-import audit thread"),
                named.join().expect("named-export audit thread"),
                shape.join().expect("binding-shape audit thread"),
            )
        });
    audit.extend(parse_findings);
    audit.extend(relative_findings);
    audit.extend(named_findings);
    audit.extend(shape_findings);
    mark_timing!("parse+relative+named+shape audits (parallel)");
    audit.extend(audit_namespace_object_member_consistency(
        &plan,
        &emitted_project,
    ));
    mark_timing!("namespace_audit");
    audit.extend(audit_module_file_sizes(&plan));
    mark_timing!("module_file_size_audit");
    let accepted_project = pre_accept.clone().accept_if_clean(&audit);
    // Planner-marked unmodularized recovered-code files (e.g. the eager
    // entrypoint island): owned by no module, but their declarations are
    // first-party naming work and must enter the symbol index.
    let unmodularized_code_paths = plan
        .files
        .iter()
        .filter(|file| file.unmodularized_recovered_code)
        .map(|file| file.path.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let symbol_index = build_symbol_index(
        &program,
        &module_output_paths,
        &emitted_project,
        &unmodularized_code_paths,
        &options.local_binding_renames,
        &options.package_anchored_island_bindings,
    );
    mark_timing!("symbol_index");
    if timing_enabled {
        let _ = last;
    }

    Ok(OutputRun {
        project: pre_accept,
        accepted_project,
        pre_accept_report: Some(pre_accept_report),
        audit,
        runtime_dependencies,
        assets,
        source_mirror_assets,
        module_output_paths,
        symbol_index,
        unmodularized_code_paths,
        island_clusters: plan.island_clusters.clone(),
    })
}

fn apply_local_binding_renames(
    plan: &mut reverts_planner::EmitPlan,
    renames: &[LocalBindingRename],
) {
    if renames.is_empty() {
        return;
    }
    let mut by_path = std::collections::BTreeMap::<&str, Vec<&LocalBindingRename>>::new();
    for rename in renames {
        by_path
            .entry(rename.file_path.as_str())
            .or_default()
            .push(rename);
    }
    for file in &mut plan.files {
        let Some(file_renames) = by_path.get(file.path.as_str()) else {
            continue;
        };
        for rename in file_renames {
            let original = BindingName::new(rename.original_name.clone());
            let semantic = BindingName::new(rename.semantic_name.clone());
            let planned = if let Some(binding_index) = rename.binding_index {
                reverts_planner::PlannedRename::new_binding_index(original, semantic, binding_index)
            } else {
                reverts_planner::PlannedRename::new_all_scopes(original, semantic)
            };
            file.add_readability_rename(planned);
        }
    }
}

fn apply_function_param_renames_to_plan(
    plan: &mut reverts_planner::EmitPlan,
    renames: &[FunctionParamRenameRow],
) {
    if renames.is_empty() {
        return;
    }
    let mut by_path = std::collections::BTreeMap::<&str, Vec<&FunctionParamRenameRow>>::new();
    for rename in renames {
        by_path
            .entry(rename.file_path.as_str())
            .or_default()
            .push(rename);
    }
    for file in &mut plan.files {
        let Some(file_renames) = by_path.get(file.path.as_str()) else {
            continue;
        };
        for rename in file_renames {
            file.add_function_param_rename(reverts_planner::PlannedParamRename {
                function: rename.function_name.clone(),
                param_index: rename.param_index,
                renamed: rename.semantic_name.clone(),
            });
        }
    }
}

/// Generate the pre-accept project needed by diagnostic inventory commands
/// without running the expensive post-emit acceptance audits. The normal
/// project generation path remains the authority for accepted output; this
/// diagnostic path only needs emitted files plus pre-planning/emit findings so
/// it can measure runtime inventory on large projects without timing out.
pub fn generate_project_inventory_from_prepared(
    prepared: PreparedProgram,
) -> Result<OutputRun, PipelineError> {
    let PreparedProgram {
        program,
        bundle_audit,
        enrichment_audit,
    } = prepared;
    let mut audit = bundle_audit;
    audit.extend(enrichment_audit);
    let input = program.model().input();
    let mut runtime_dependencies = collect_runtime_dependencies(input);
    extend_runtime_dependencies_with_island_externalizations(&mut runtime_dependencies, &program);
    let asset_references = collect_required_asset_references(input);
    let assets = collect_emitted_assets(input, &asset_references);
    audit.extend(audit_required_sources(&program));
    audit.extend(audit_required_assets(input, &asset_references));
    if audit.has_errors() {
        return Ok(OutputRun {
            project: PreAcceptProject::empty(),
            accepted_project: None,
            pre_accept_report: None,
            audit,
            runtime_dependencies,
            assets,
            source_mirror_assets: Vec::new(),
            module_output_paths: BTreeMap::new(),
            symbol_index: Vec::new(),
            unmodularized_code_paths: std::collections::BTreeSet::new(),
            island_clusters: Vec::new(),
        });
    }

    let planner = ImportExportPlanner;
    let plan = planner
        .plan_enriched_program(&program)
        .map_err(PipelineError::Plan)?;
    audit.extend(audit_emit_plan_synthesis(&plan));
    if audit.has_errors() {
        return Ok(OutputRun {
            project: PreAcceptProject::empty(),
            accepted_project: None,
            pre_accept_report: None,
            audit,
            runtime_dependencies,
            assets,
            source_mirror_assets: Vec::new(),
            module_output_paths: BTreeMap::new(),
            symbol_index: Vec::new(),
            unmodularized_code_paths: std::collections::BTreeSet::new(),
            island_clusters: Vec::new(),
        });
    }

    let module_output_paths = module_output_paths(&program);
    let validated_plan = plan.clone().validate().map_err(PipelineError::Plan)?;
    let outcome = emit_validated_project(&validated_plan).map_err(PipelineError::Emit)?;
    for finding in outcome.findings {
        audit.push(finding);
    }
    let pre_accept = pre_accept::apply_pre_accept_transforms(
        outcome.project,
        &pre_accept::PreAcceptContext {
            input,
            asset_references: &asset_references,
            module_output_paths: &module_output_paths,
        },
    );
    let pre_accept_report = pre_accept.report.clone();

    let unmodularized_code_paths = plan
        .files
        .iter()
        .filter(|file| file.unmodularized_recovered_code)
        .map(|file| file.path.clone())
        .collect::<std::collections::BTreeSet<_>>();
    Ok(OutputRun {
        project: pre_accept,
        accepted_project: None,
        pre_accept_report: Some(pre_accept_report),
        audit,
        runtime_dependencies,
        assets,
        source_mirror_assets: Vec::new(),
        module_output_paths,
        symbol_index: Vec::new(),
        unmodularized_code_paths,
        island_clusters: plan.island_clusters.clone(),
    })
}

fn missing_module_source_file_ids(input: &InputBundle) -> HashSet<u32> {
    let has_persisted_bundle_split = input.modules.iter().any(|module| {
        module.original_name.starts_with("esbuild:")
            || module.original_name.starts_with("webpack:")
            || module.original_name.starts_with("rollup:")
    });
    let attached: HashSet<u32> = input
        .modules
        .iter()
        .filter_map(|module| module.source_file_id)
        .collect();
    input
        .source_files
        .iter()
        .filter_map(|source_file| {
            if attached.contains(&source_file.id) {
                return None;
            }
            if has_persisted_bundle_split
                && source_file.path.ends_with(".js")
                && source_file
                    .source
                    .as_deref()
                    .is_some_and(|source| source.contains("@bun @bytecode @bun-cjs"))
            {
                return None;
            }
            Some(source_file.id)
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineError {
    Input(InputBundleError),
    Plan(PlanError),
    Emit(EmitError),
}

impl fmt::Display for PipelineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(error) => write!(formatter, "{error}"),
            Self::Plan(error) => write!(formatter, "{error}"),
            Self::Emit(error) => write!(formatter, "{error}"),
        }
    }
}

impl Error for PipelineError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Input(source) => Some(source),
            Self::Plan(source) => Some(source),
            Self::Emit(source) => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use reverts_analyze::enrich_program;
    use reverts_emitter::emit_project;
    use reverts_input::{
        AssetInput, AssetKind, InputBundle, InputRows, ModuleDependencyInput,
        ModuleDependencyTarget, ModuleInput, PackageAttributionInput, PackageSurfaceInput,
        ProjectInput, SourceFileInput, SourceSpan, SymbolInput,
    };
    use reverts_ir::{
        BindingName, BindingShape, BindingSourceKind, BindingUseKind, ModuleId, ModuleKind,
    };
    use reverts_js::{ParseGoal, parse_source};
    use reverts_model::ProgramModel;
    use reverts_observe::{FindingCode, Severity};
    use reverts_planner::{EmitPlan, ImportExportPlanner, PlannedBinding, PlannedFile};

    use std::collections::{BTreeMap, BTreeSet};

    use reverts_emitter::{EmittedFile, EmittedProject};

    use super::assets::{collect_dynamic_asset_references, current_node_platform_dir};
    use super::audit::{audit_emit_plan_synthesis, audit_namespace_object_member_consistency};
    use super::source_rewrites::fold_multiline_static_template_literals_in_source;
    use super::{
        GenerateProjectOptions, LocalBindingRename, OutputRun, generate_project_from_input,
        generate_project_from_input_with_options, prepare_input_bundle_for_generation,
        prepare_input_rows_for_pipeline,
    };

    fn rows_with_application_module() -> InputRows {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "app", "src/index.ts"));
        rows
    }

    fn rows_with_application_source(source: &str) -> InputRows {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some(source.to_string()),
        ));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1));
        rows
    }

    #[test]
    fn complete_fixture_generates_parseable_project_with_package_match_and_semantic_names() {
        let mut rows = rows_with_application_source("export function activate() { return 42; }");
        rows.modules.push(ModuleInput::package(
            ModuleId(2),
            "lodash_map",
            "node_modules/lodash/map.js",
            "lodash",
            Some("4.17.21".to_string()),
        ));
        rows.symbols.push(SymbolInput::new(ModuleId(1), "activate"));
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

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        assert_eq!(run.project.files.len(), 1);
        assert_eq!(run.runtime_dependencies.len(), 1);
        assert_eq!(run.runtime_dependencies[0].package_name, "lodash");
        assert_eq!(run.runtime_dependencies[0].package_version, "4.17.21");
        let source = run.project.files[0].source.as_str();
        assert!(source.contains("import * as lodashMap from 'lodash/map';"));
        assert!(source.contains("export function activate()"));
        assert!(!source.contains("undefined as any"));
    }

    #[test]
    fn local_binding_renames_apply_during_emission() {
        let rows =
            rows_with_application_source("export function run(a) { const b = a + 1; return b; }");
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input_with_options(
            input,
            GenerateProjectOptions {
                local_binding_renames: vec![
                    LocalBindingRename {
                        file_path: "src/index.ts".to_string(),
                        original_name: "a".to_string(),
                        binding_index: None,
                        semantic_name: "inputValue".to_string(),
                    },
                    LocalBindingRename {
                        file_path: "src/index.ts".to_string(),
                        original_name: "b".to_string(),
                        binding_index: None,
                        semantic_name: "resultValue".to_string(),
                    },
                ],
                function_param_renames: Vec::new(),
                package_anchored_island_bindings: std::collections::BTreeSet::new(),
            },
        )
        .expect("fixture should emit");

        assert!(run.audit.is_clean());
        let source = run.project.files[0].source.as_str();
        // Type inference marks the unconstrained param optional (`inputValue?`).
        assert!(source.contains("run(inputValue?)"));
        assert!(source.contains("const resultValue = inputValue + 1;"));
        assert!(source.contains("return resultValue;"));
    }

    #[test]
    fn literal_type_solution_annotates_top_level_const_output() {
        let rows = rows_with_application_source("export const answer = 42;");
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        let source = run.project.files[0].source.as_str();
        assert!(
            source.contains("export const answer: number = 42;"),
            "{source}"
        );
        parse_source(
            source,
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
        )
        .expect("annotated output must remain parseable TypeScript");
    }

    #[test]
    fn package_surface_contributes_runtime_dependency_without_package_module() {
        let mut rows =
            rows_with_application_source("const undici = require('undici'); export { undici };");
        rows.package_surfaces
            .push(PackageSurfaceInput::accepted_external(
                "undici", "2.2.1", "undici",
            ));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        assert_eq!(run.runtime_dependencies.len(), 1);
        assert_eq!(run.runtime_dependencies[0].package_name, "undici");
        assert_eq!(run.runtime_dependencies[0].package_version, "2.2.1");
        assert!(run.project.files[0].source.contains("require('undici')"));
    }

    #[test]
    fn runtime_dependencies_prefer_highest_exact_package_version() {
        let mut rows = rows_with_application_source("export const value = 1;");
        rows.modules.push(ModuleInput::package(
            ModuleId(2),
            "otelCoreOld",
            "node_modules/@opentelemetry/core/old.js",
            "@opentelemetry/core",
            Some("1.0.1".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(3),
            "otelCoreNew",
            "node_modules/@opentelemetry/core/new.js",
            "@opentelemetry/core",
            Some("1.30.0".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(4),
            "pkgRange",
            "node_modules/pkg/range.js",
            "pkg",
            Some("1.x".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(5),
            "pkgExact",
            "node_modules/pkg/exact.js",
            "pkg",
            Some("1.2.3".to_string()),
        ));
        rows.package_attributions
            .push(PackageAttributionInput::accepted_external(
                ModuleId(2),
                "@opentelemetry/core",
                "1.0.1",
                "@opentelemetry/core",
            ));
        rows.package_attributions
            .push(PackageAttributionInput::accepted_external(
                ModuleId(3),
                "@opentelemetry/core",
                "1.30.0",
                "@opentelemetry/core",
            ));
        rows.package_attributions
            .push(PackageAttributionInput::accepted_external(
                ModuleId(4),
                "pkg",
                "1.x",
                "pkg",
            ));
        rows.package_attributions
            .push(PackageAttributionInput::accepted_external(
                ModuleId(5),
                "pkg",
                "1.2.3",
                "pkg",
            ));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        let dependencies = run
            .runtime_dependencies
            .iter()
            .map(|dependency| {
                (
                    dependency.package_name.as_str(),
                    dependency.package_version.as_str(),
                )
            })
            .collect::<BTreeSet<_>>();
        assert!(dependencies.contains(&("@opentelemetry/core", "1.30.0")));
        assert!(dependencies.contains(&("pkg", "1.2.3")));
    }

    #[test]
    fn pipeline_carries_input_assets_to_output_run() {
        let mut rows = rows_with_application_source(
            "const rgPath = require('/$bunfs/root/vendor/rg'); export { rgPath };",
        );
        rows.assets.push(AssetInput::new(
            100,
            "/$bunfs/root/vendor/rg",
            "modules/1-app/vendor/rg",
            b"rg-binary".to_vec(),
            AssetKind::Executable,
            true,
        ));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        assert_eq!(run.assets.len(), 1);
        assert_eq!(run.assets[0].path, "modules/1-app/vendor/rg");
        assert_eq!(run.assets[0].bytes, b"rg-binary");
        assert!(run.assets[0].executable);
    }

    #[test]
    fn pipeline_folds_multiline_static_template_literals_without_runtime_io() {
        let docs = [
            "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india",
        ]
        .join("\n");
        let source = format!("var docs = `{docs}`;\nexport {{ docs }};\n");
        let mut rows = rows_with_application_source(source.as_str());
        rows.symbols.push(SymbolInput::new(ModuleId(1), "docs"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            emitted.contains("var docs: string = \"alpha\\nbravo\\ncharlie\\ndelta"),
            "multiline static template must be a regular escaped string, got:\n{emitted}",
        );
        assert!(
            !emitted.contains("readFileSync") && !emitted.contains("node:fs"),
            "line folding must not introduce runtime I/O dependencies, got:\n{emitted}",
        );
        assert!(
            emitted.lines().count() < source.lines().count(),
            "folded source should have fewer lines; before={}, after={}\n{emitted}",
            source.lines().count(),
            emitted.lines().count(),
        );
        parse_source(
            emitted,
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
        )
        .expect("folded output should remain parseable");
    }

    #[test]
    fn static_template_folding_keeps_single_line_interpolated_and_tagged_templates() {
        let source = "const short = `one\\ntwo`;\n\
                      const interpolated = `hello ${name}`;\n\
                      const tagged = tag`a\nb\nc\nd\ne\nf\ng\nh`;\n";

        let folded = fold_multiline_static_template_literals_in_source(source, "fixture.ts");

        assert!(
            folded.contains("const short = `one\\ntwo`;"),
            "short one-line templates are not a line-count problem: {folded}"
        );
        assert!(
            folded.contains("const interpolated = `hello ${name}`;"),
            "interpolated templates are not a static string: {folded}"
        );
        assert!(
            folded.contains("tag`a\nb\nc\nd\ne\nf\ng\nh`"),
            "tagged templates expose raw/cooked arrays and must be preserved: {folded}"
        );
    }

    #[test]
    fn pipeline_warns_on_asset_reference_missing_from_project_assets() {
        let rows = rows_with_application_source(
            "const native = require('/$bunfs/root/addon.node'); export { native };",
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should return audit");

        assert!(run.audit.has(FindingCode::MissingRequiredAsset));
        // Per ADR 0002 the missing asset is an input-bundle condition; emit
        // the project anyway and let the audit surface what's missing.
        assert!(
            !run.project.files.is_empty(),
            "missing-asset is now a warning; project should still emit"
        );
        assert!(run.assets.is_empty());
        let finding = run
            .audit
            .findings()
            .iter()
            .find(|finding| finding.code == FindingCode::MissingRequiredAsset)
            .expect("missing-asset finding present");
        assert_eq!(finding.severity, Severity::Warning);
        assert_eq!(finding.module.as_deref(), Some("1"));
        assert_eq!(finding.binding.as_deref(), Some("/$bunfs/root/addon.node"));
    }

    #[test]
    fn pipeline_preserves_unreferenced_project_assets() {
        let mut rows = rows_with_application_source(
            "const native = require('/$bunfs/root/addon.node'); export { native };",
        );
        rows.assets.push(AssetInput::new(
            100,
            "/$bunfs/root/addon.node",
            "modules/1-app/addon.node",
            b"native".to_vec(),
            AssetKind::NativeNode,
            false,
        ));
        rows.assets.push(AssetInput::new(
            101,
            "/$bunfs/root/unused.node",
            "modules/1-app/unused.node",
            b"unused".to_vec(),
            AssetKind::NativeNode,
            false,
        ));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        assert_eq!(run.assets.len(), 2);
        assert_eq!(run.assets[0].path, "modules/1-app/addon.node");
        assert_eq!(run.assets[0].bytes, b"native");
        assert_eq!(run.assets[1].path, "modules/1-app/unused.node");
        assert_eq!(run.assets[1].bytes, b"unused");
    }

    #[test]
    fn asset_audit_ignores_inert_string_literals_and_bare_commands() {
        let rows = rows_with_application_source(
            "const message = 'install bash.exe from C:\\\\Program Files\\\\Git\\\\bin\\\\bash.exe';\n\
             const cssSuffix = '.bundle.css';\n\
             const command = 'rg';\n\
             export { message, cssSuffix, command };",
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        assert!(run.assets.is_empty());
    }

    #[test]
    fn asset_audit_recovers_dynamic_ripgrep_vendor_binary_path() {
        let source = "\
            const root = path.resolve(base, 'vendor', 'ripgrep');\n\
            const command = path.resolve(root, `${process.arch}-${process.platform}`, 'rg');\n\
            export { command };";
        let references = collect_dynamic_asset_references(source, "fixture.ts");
        let Some(platform_dir) = current_node_platform_dir() else {
            return;
        };

        assert_eq!(
            references,
            vec![format!("vendor/ripgrep/{platform_dir}/rg")]
        );
    }

    #[test]
    fn asset_audit_recovers_hardcoded_ripgrep_vendor_binary_path_from_path_builders() {
        let source = "\
            const root = ODH.resolve(base, 'vendor', 'ripgrep');\n\
            const command = ODH.resolve(root, 'x64-linux', 'rg');\n\
            const inert = ['vendor', 'ripgrep', 'rg'];\n\
            export { command };";

        let references = collect_dynamic_asset_references(source, "fixture.ts");

        assert_eq!(references, vec!["vendor/ripgrep/x64-linux/rg".to_string()]);
    }

    #[test]
    fn asset_audit_keeps_ripgrep_binary_name_bound_to_its_platform_call() {
        let source = "\
            const root = path.resolve(base, 'vendor', 'ripgrep');\n\
            const command = process.platform === 'win32'\n\
                ? path.resolve(root, 'x64-win32', 'rg.exe')\n\
                : path.resolve(root, `${process.arch}-${process.platform}`, 'rg');\n\
            export { command };";
        let references = collect_dynamic_asset_references(source, "fixture.ts");
        let Some(platform_dir) = current_node_platform_dir() else {
            return;
        };

        assert_eq!(
            references,
            vec![
                format!("vendor/ripgrep/{platform_dir}/rg"),
                "vendor/ripgrep/x64-win32/rg.exe".to_string(),
            ]
        );
        assert!(
            !references
                .iter()
                .any(|reference| reference == "vendor/ripgrep/x64-win32/rg")
        );
    }

    #[test]
    fn pipeline_rewrites_matched_asset_literals_to_relative_emitted_paths() {
        let mut rows = rows_with_application_source(
            "const native = require('/$bunfs/root/addon.node'); export { native };",
        );
        rows.assets.push(AssetInput::new(
            100,
            "/$bunfs/root/addon.node",
            "src/addon.node",
            b"native".to_vec(),
            AssetKind::NativeNode,
            false,
        ));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        let source = run.project.files[0].source.as_str();
        assert!(source.contains("require('./addon.node')"));
        assert!(!source.contains("/$bunfs/root/addon.node"));
        assert_eq!(run.assets[0].path, "src/addon.node");
    }

    #[test]
    fn pipeline_recognizes_asset_reference_via_new_url_with_import_meta_url() {
        // The `new URL('X', import.meta.url)` idiom is the modern ESM way to
        // reference a sibling asset (wasm, native bindings). The detector
        // must accept the first-argument string as an asset specifier.
        let mut rows = rows_with_application_source(
            "const wasm = new URL('/$bunfs/root/parser.wasm', import.meta.url);\n\
             export { wasm };",
        );
        rows.assets.push(AssetInput::new(
            100,
            "/$bunfs/root/parser.wasm",
            "src/parser.wasm",
            b"\x00asm\x01\x00\x00\x00".to_vec(),
            AssetKind::Wasm,
            false,
        ));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings(),);
        assert_eq!(run.assets.len(), 1);
        assert_eq!(run.assets[0].path, "src/parser.wasm");
        let source = run.project.files[0].source.as_str();
        assert!(
            source.contains("new URL('./parser.wasm', import.meta.url)"),
            "new URL specifier must be rewritten to relative path; got:\n{source}",
        );
    }

    #[test]
    fn pipeline_recognizes_asset_reference_via_static_import_statement() {
        // ESM modules commonly reference native bindings via static imports
        // rather than CommonJS `require`. The asset detector must accept the
        // import specifier as an asset reference and rewrite it.
        let mut rows = rows_with_application_source(
            "import addon from '/$bunfs/root/addon.node';\nexport { addon };",
        );
        rows.assets.push(AssetInput::new(
            100,
            "/$bunfs/root/addon.node",
            "src/addon.node",
            b"native".to_vec(),
            AssetKind::NativeNode,
            false,
        ));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings(),);
        assert_eq!(run.assets.len(), 1);
        let source = run.project.files[0].source.as_str();
        assert!(
            source.contains("from './addon.node'"),
            "static import must be rewritten to relative path; got:\n{source}",
        );
        assert!(
            !source.contains("/$bunfs/root/addon.node"),
            "rewritten source must not retain original logical path; got:\n{source}",
        );
    }

    #[test]
    fn pipeline_emits_shared_asset_once_when_referenced_by_multiple_modules() {
        // Two application modules reference the same on-disk asset. The
        // pipeline must:
        //   - emit the asset bytes exactly once,
        //   - rewrite the literal in BOTH modules to a relative path.
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/loader-a.ts",
            Some("const a = require('/$bunfs/root/addon.node'); export { a };".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            2,
            "src/loader-b.ts",
            Some("const b = require('/$bunfs/root/addon.node'); export { b };".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "loader_a", "src/loader-a.ts")
                .with_source_file(1),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "loader_b", "src/loader-b.ts")
                .with_source_file(2),
        );
        rows.assets.push(AssetInput::new(
            100,
            "/$bunfs/root/addon.node",
            "src/addon.node",
            b"native".to_vec(),
            AssetKind::NativeNode,
            false,
        ));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        assert_eq!(
            run.assets.len(),
            1,
            "shared asset must not be emitted multiple times: {:?}",
            run.assets,
        );
        assert_eq!(run.assets[0].path, "src/addon.node");
        assert_eq!(run.assets[0].bytes, b"native");

        assert_eq!(run.project.files.len(), 2);
        for file in &run.project.files {
            assert!(
                file.source.contains("require('./addon.node')"),
                "module {} must rewrite to relative path; got:\n{}",
                file.path,
                file.source,
            );
            assert!(
                !file.source.contains("/$bunfs/root/addon.node"),
                "module {} must not retain the original logical path; got:\n{}",
                file.path,
                file.source,
            );
        }
    }

    #[test]
    fn pipeline_canonicalizes_build_file_url_source_locations_for_asset_paths() {
        let mut rows = rows_with_application_source(
            "\
            const POL = { fileURLToPath(value) { return value; } };\n\
            const ODH = { join(...parts) { return parts.join('/'); }, resolve(...parts) { return parts.join('/'); } };\n\
            const yL9 = POL.fileURLToPath('file:///home/runner/work/claude-cli-internal/claude-cli-internal/src/utils/ripgrep.ts');\n\
            const hL9 = ODH.join(yL9, '../');\n\
            const root = ODH.resolve(hL9, 'vendor', 'ripgrep');\n\
            const command = ODH.resolve(root, 'x64-linux', 'rg');\n\
            export { command };",
        );
        rows.assets.push(AssetInput::new(
            100,
            "vendor/ripgrep/x64-linux/rg",
            "src/vendor/ripgrep/x64-linux/rg",
            b"rg".to_vec(),
            AssetKind::Executable,
            true,
        ));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        assert_eq!(run.assets.len(), 1);
        assert_eq!(run.assets[0].path, "src/vendor/ripgrep/x64-linux/rg");
        let source = run.project.files[0].source.as_str();
        assert!(source.contains("POL.fileURLToPath(import.meta.url)"));
        assert!(!source.contains("/home/runner/work/claude-cli-internal"));
    }

    #[test]
    fn source_backed_package_import_is_not_emitted_twice() {
        let mut rows = rows_with_application_source(
            "import { map } from 'lodash/map'; export const answer = map;",
        );
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

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        let source = run.project.files[0].source.as_str();
        assert!(source.contains("import { map } from 'lodash/map';"));
        assert!(!source.contains("__pkg_lodash_map"));
        assert_eq!(source.matches("from 'lodash/map'").count(), 1);
    }

    #[test]
    fn source_backed_semantic_name_is_renamed_late_before_emit() {
        let mut rows =
            rows_with_application_source("var $F1 = 1; console.log($F1); export { $F1 };");
        rows.symbols.push(
            SymbolInput::new(ModuleId(1), "$F1").with_semantic_name("lodashGlobalObjectInit"),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        let source = run.project.files[0].source.as_str();
        assert!(source.contains("var lodashGlobalObjectInit: number = 1;"));
        assert!(source.contains("console.log(lodashGlobalObjectInit);"));
        // The binding is exported, has a project-wide-unique semantic name, and
        // is not re-exported or namespace-imported, so the wire-rename pass also
        // renames the public export name (no `as $F1` alias to preserve).
        assert!(source.contains("export { lodashGlobalObjectInit };"));
        assert!(!source.contains("console.log($F1);"));
        assert!(!source.contains("as $F1"));
    }

    #[test]
    fn alias_source_binding_is_preserved_when_unhinted() {
        let mut rows =
            rows_with_application_source("var a = 1; var b = a; console.log(b); export { b };");
        rows.symbols
            .push(SymbolInput::new(ModuleId(1), "a").with_semantic_name("settings"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        let source = run.project.files[0].source.as_str();
        assert!(source.contains("var settings: number = 1;"));
        assert!(source.contains("var b = settings;"));
        assert!(source.contains("console.log(b);"));
        assert!(source.contains("export { b };"));
    }

    #[test]
    fn source_named_import_alias_is_cleaned_up_late_before_emit() {
        let mut rows = rows_with_application_source(
            "import { map as $F1 } from 'lodash/map'; console.log($F1); export { $F1 };",
        );
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

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        let source = run.project.files[0].source.as_str();
        assert!(source.contains("import { map } from 'lodash/map';"));
        assert!(source.contains("console.log(map);"));
        assert!(source.contains("export { map as $F1 };"));
        assert!(!source.contains("console.log($F1);"));
    }

    #[test]
    fn commonjs_export_property_drives_late_name_and_function_recovery() {
        let rows = rows_with_application_source(
            "const a = function() { return 1; }; exports.createClient = a;",
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        let source = run.project.files[0].source.as_str();
        assert!(source.contains("function createClient()"));
        assert!(source.contains("exports.createClient = createClient;"));
        assert!(!source.contains("const a = function"));
    }

    #[test]
    fn module_exports_object_drives_late_name_and_shorthand() {
        let rows =
            rows_with_application_source("const a = 1; module.exports = { createClient: a };");
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        let source = run.project.files[0].source.as_str();
        // `module.exports = { createClient: a }` drives the late name `a` →
        // `createClient`; the CommonJS→ESM lowering then emits the object as a
        // shorthand ESM re-export of the (renamed) binding.
        assert!(
            source.contains("const createClient: number = 1;"),
            "SRC:\n{source}"
        );
        assert!(
            source.contains("export { createClient };"),
            "SRC:\n{source}"
        );
        assert!(!source.contains("module.exports"), "SRC:\n{source}");
    }

    #[test]
    fn export_hint_wins_over_later_object_property_hint() {
        let rows = rows_with_application_source(
            "const a = 1; export { a as createClient }; const obj = { internalName: a };",
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        let source = run.project.files[0].source.as_str();
        assert!(source.contains("const createClient: number = 1;"));
        assert!(source.contains("export { createClient };"));
        assert!(source.contains("const obj = { internalName: createClient };"));
        assert!(!source.contains("const internalName = 1;"));
    }

    #[test]
    fn readability_polish_recovers_destructuring_and_usage_names_in_pipeline() {
        let rows = rows_with_application_source(
            "const api = { createClient() { return 1; }, close() {} }; const client = api.createClient; const close = api.close; class Logger {}; const a = new Logger(); console.log(client, close, a);",
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean(), "{:?}", run.audit.findings());
        let source = run.project.files[0].source.as_str();
        assert!(source.contains("const { createClient: client, close } = api;"));
        assert!(source.contains("const logger = new Logger();"));
        assert!(source.contains("console.log(client, close, logger);"));
    }

    #[test]
    fn duplicate_named_imports_are_merged_and_sorted_late() {
        let rows = rows_with_application_source(
            "import { z } from './utils'; import { a } from './utils'; console.log(z, a);",
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        let source = run.project.files[0].source.as_str();
        assert_eq!(source.matches("from './utils'").count(), 1);
        assert!(source.contains("import { a, z } from './utils';"));
        assert!(source.contains("console.log(z, a);"));
    }

    #[test]
    fn namespace_imports_are_split_and_merged_late_when_safe() {
        let rows = rows_with_application_source(
            "import * as utils from './utils'; import { a } from './utils'; console.log(utils.z, a);",
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        let source = run.project.files[0].source.as_str();
        assert_eq!(source.matches("from './utils'").count(), 1);
        assert!(source.contains("import { a, z } from './utils';"));
        assert!(source.contains("console.log(z, a);"));
        assert!(!source.contains("utils.z"));
    }

    #[test]
    fn late_readability_rename_keeps_original_name_when_semantic_collides() {
        let mut rows = rows_with_application_source(
            "var a = 1; var settings = 2; console.log(a, settings); export { a };",
        );
        rows.symbols
            .push(SymbolInput::new(ModuleId(1), "a").with_semantic_name("settings"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        let source = run.project.files[0].source.as_str();
        assert!(source.contains("var a: number = 1;"));
        assert!(source.contains("var settings: number = 2;"));
        assert!(source.contains("console.log(a, settings);"));
        assert!(source.contains("export { a };"));
    }

    #[test]
    fn commonjs_require_package_import_uses_surface_attribution_without_synthetic_import() {
        let mut rows = rows_with_application_source(
            "const add = require('pkg/add'); export const total = add(1, 2);",
        );
        rows.modules.push(ModuleInput::package(
            ModuleId(2),
            "pkg_add",
            "node_modules/pkg/add.js",
            "pkg",
            Some("1.2.3".to_string()),
        ));
        rows.package_attributions.push(
            PackageAttributionInput::accepted_external(ModuleId(2), "pkg", "1.2.3", "pkg/add")
                .with_subpath("add"),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        let source = run.project.files[0].source.as_str();
        assert!(source.contains("require('pkg/add')"));
        assert!(!source.contains("__pkg_pkg_add"));
    }

    #[test]
    fn source_backed_default_export_does_not_require_synthetic_default_binding() {
        let rows = rows_with_application_source("export default () => 42;");
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        assert_eq!(run.project.files.len(), 1);
        assert!(
            run.project.files[0]
                .source
                .contains("export default () => 42")
        );
    }

    #[test]
    fn paper_aligned_pipeline_wires_graph_shape_package_plan_emit_and_parse() {
        let mut rows = rows_with_application_source(
            r#"
                import { factory } from 'pkg/factory';
                const bag = { value: 1 };
                function local() {
                    return factory(bag.value);
                }
                local();
                export { local };
            "#,
        );
        rows.modules.push(ModuleInput::package(
            ModuleId(2),
            "pkg_factory",
            "node_modules/pkg/factory.js",
            "pkg",
            Some("1.2.3".to_string()),
        ));
        rows.package_attributions.push(
            PackageAttributionInput::accepted_external(ModuleId(2), "pkg", "1.2.3", "pkg/factory")
                .with_subpath("factory"),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let model = ProgramModel::from_input(input);
        let graph = model.graph();
        assert!(graph.ast_errors().is_empty());
        assert!(
            graph
                .import_export()
                .package_imports_for(ModuleId(1))
                .contains(&"pkg/factory")
        );
        assert!(
            graph
                .import_export()
                .exports_for(ModuleId(1))
                .iter()
                .any(|binding| binding.as_str() == "local")
        );
        assert!(graph.def_use().unresolved_reads().is_empty());
        assert!(graph.def_use().unresolved_writes().is_empty());
        let dependence_edges = graph.def_use().data_dependence_edges();
        assert!(dependence_edges.iter().any(|edge| {
            edge.binding.as_str() == "factory"
                && edge.source == BindingSourceKind::Import
                && edge.target == BindingUseKind::Read
        }));
        assert!(dependence_edges.iter().any(|edge| {
            edge.binding.as_str() == "bag"
                && edge.source == BindingSourceKind::Definition
                && edge.target == BindingUseKind::Read
        }));

        let enrichment = enrich_program(model);
        assert!(enrichment.audit.is_clean());
        let program = enrichment.program;
        assert_eq!(
            program.binding_shape(ModuleId(1), "factory"),
            BindingShape::Callable
        );
        assert_eq!(
            program.binding_shape(ModuleId(1), "local"),
            BindingShape::Callable
        );
        assert_eq!(
            program.binding_shape(ModuleId(1), "bag"),
            BindingShape::NamespaceObject
        );
        assert_eq!(program.package_imports().len(), 1);
        assert!(program.package_imports()[0].source_backed);
        assert_eq!(
            program.package_imports()[0].resolution.specifier(),
            Some("pkg/factory")
        );

        let plan = ImportExportPlanner
            .plan_enriched_program(&program)
            .expect("paper-aligned fixture should plan");
        let file = &plan.files[0];
        assert!(
            file.imports.iter().any(|import| import.source_backed
                && import.resolution.specifier() == Some("pkg/factory"))
        );
        assert!(
            file.bindings
                .iter()
                .any(|binding| binding.original.as_str() == "factory" && binding.source_backed)
        );
        assert!(
            file.exports
                .iter()
                .any(|export| export.binding.as_str() == "local" && export.source_backed)
        );

        let project = emit_project(&plan)
            .expect("paper-aligned fixture should emit")
            .project;
        let source = project.files[0].source.as_str();
        assert!(source.contains("import { factory } from 'pkg/factory';"));
        assert!(source.contains("function local()"));
        assert!(source.contains("export { local };"));
        assert!(!source.contains("__pkg_pkg_factory"));
        parse_source(
            source,
            Some(Path::new(project.files[0].path.as_str())),
            ParseGoal::TypeScript,
        )
        .expect("emitted fixture should parse");
    }

    #[test]
    fn pipeline_wires_arbitrary_bundle_prelude_runtime_helpers_end_to_end() {
        let prelude = concat!(
            "var $wrap7 = (factory, cache) => () => ",
            "(cache || factory((cache = { exports: {} }).exports, cache), cache.exports);\n",
            "var _lazy9 = (init, cache) => () => (init && (cache = init(init = 0)), cache);\n",
        );
        let body = concat!(
            "var entry = $wrap7((exports, module) => { module.exports = 1; });\n",
            "var init = _lazy9(() => { entry(); });\n",
            "init();\n",
            "export { entry };\n",
        );
        let source = format!("{prelude}{body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        // The CommonJS wrapper is local now, so this fixture no longer needs a
        // shared runtime helper file at all.
        assert_eq!(run.project.files.len(), 1);
        let entry = run
            .project
            .files
            .iter()
            .find(|file| file.path == "modules/entry.ts")
            .expect("entry file should be emitted");
        assert!(!entry.source.contains("$wrap7"));
        assert!(!entry.source.contains("_lazy9"));
        assert!(!entry.source.contains("lazyModule("));
        assert!(!entry.source.contains("lazyValue("));
        // `init`'s body (`entry();`) has no top-level return and `init` is
        // invoked, so the global de-lazify post-pass hoists it to eager
        // module-eval, stubs it, and drops the now-dead local `_$l` memoizer.
        assert!(!entry.source.contains("_$l"));
        assert!(entry.source.contains("entry();"));
        assert!(entry.source.contains("function init() {}"));
        // The CommonJS memoization temps remain local to the recovered module.
        assert!(entry.source.contains("_$cached"));
        assert!(!entry.source.contains("_$init"));
        assert!(
            run.project
                .files
                .iter()
                .all(|file| file.path != "modules/runtime/source-1-helpers.ts")
        );
    }

    #[test]
    fn source_backed_reexports_stay_on_package_surface_without_synthetic_namespace() {
        let mut rows = rows_with_application_source(
            "export { map as lodashMap } from 'lodash/map';\nexport * as fp from 'lodash/fp';",
        );
        rows.modules.push(ModuleInput::package(
            ModuleId(2),
            "lodash_map",
            "node_modules/lodash/map.js",
            "lodash",
            Some("4.17.21".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(3),
            "lodash_fp",
            "node_modules/lodash/fp.js",
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
        rows.package_attributions.push(
            PackageAttributionInput::accepted_external(
                ModuleId(3),
                "lodash",
                "4.17.21",
                "lodash/fp",
            )
            .with_subpath("fp"),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        let source = run.project.files[0].source.as_str();
        assert!(source.contains("export { map as lodashMap } from 'lodash/map';"));
        assert!(source.contains("export * as fp from 'lodash/fp';"));
        assert!(!source.contains("__pkg_lodash"));
        assert_eq!(source.matches("from 'lodash/").count(), 2);
    }

    #[test]
    fn unknown_package_surface_reports_unresolvable_import_without_external_tools() {
        let mut rows = rows_with_application_module();
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(1),
            target: ModuleDependencyTarget::Package {
                specifier: "lodash/map".to_string(),
            },
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        // Per ADR 0002 unresolvable bare imports are an input-bundle
        // condition: the audit names them as warnings but emission proceeds.
        assert!(run.audit.has(FindingCode::UnresolvableBareImport));
        let finding = run
            .audit
            .findings()
            .iter()
            .find(|finding| finding.code == FindingCode::UnresolvableBareImport)
            .expect("unresolvable bare import finding present");
        assert_eq!(finding.severity, Severity::Warning);
    }

    #[test]
    fn missing_symbol_source_is_reported_without_emitting_generated_implementation() {
        let mut rows = rows_with_application_module();
        rows.symbols.push(SymbolInput::new(ModuleId(1), "activate"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.has(FindingCode::MissingDefinition));
        assert!(run.project.files.is_empty());
    }

    #[test]
    fn module_without_source_is_reported_as_warning() {
        let rows = rows_with_application_module();
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        // Module-without-source is an input condition (ADR 0002): warn,
        // don't strand. The emitter simply won't produce a file for the
        // module since there's no source body to back it.
        let finding = run
            .audit
            .findings()
            .iter()
            .find(|finding| {
                finding.code == FindingCode::MissingDefinition
                    && finding.module.as_deref() == Some("1")
                    && finding.binding.is_none()
            })
            .expect("missing-source finding present");
        assert_eq!(finding.severity, Severity::Warning);
    }

    #[test]
    fn duplicate_input_symbols_emit_single_missing_source_finding() {
        let mut rows = rows_with_application_module();
        rows.symbols.push(SymbolInput::new(ModuleId(1), "activate"));
        rows.symbols.push(SymbolInput::new(ModuleId(1), "activate"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert_eq!(
            run.audit
                .findings()
                .iter()
                .filter(|finding| finding.code == FindingCode::MissingDefinition
                    && finding.binding.as_deref() == Some("activate"))
                .count(),
            1
        );
        assert!(run.project.files.is_empty());
    }

    #[test]
    fn shared_bundle_source_is_not_duplicated_as_module_implementation() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some("function bundle() {}".to_string()),
        ));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "m1", "modules/m1.ts").with_source_file(1));
        rows.modules.push(ModuleInput {
            id: ModuleId(2),
            kind: ModuleKind::Application,
            original_name: "m2".to_string(),
            semantic_path: "modules/m2.ts".to_string(),
            source_file_id: Some(1),
            source_span: None,
            package_name: None,
            package_version: None,
        });
        rows.symbols.push(SymbolInput::new(ModuleId(1), "one"));
        rows.symbols.push(SymbolInput::new(ModuleId(2), "two"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.has(FindingCode::MissingDefinition));
        assert!(run.project.files.is_empty());
    }

    #[test]
    fn bundle_source_spans_emit_real_module_slices() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some("export const one = 1;\nexport const two = 2;".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "one", "modules/one.ts")
                .with_source_file(1)
                .with_source_span(reverts_input::SourceSpan::new(0, 21)),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "two", "modules/two.ts")
                .with_source_file(1)
                .with_source_span(reverts_input::SourceSpan::new(22, 43)),
        );
        rows.symbols.push(SymbolInput::new(ModuleId(1), "one"));
        rows.symbols.push(SymbolInput::new(ModuleId(2), "two"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        assert_eq!(run.project.files.len(), 2);
        assert!(
            run.project.files[0]
                .source
                .contains("export const one: number = 1")
        );
        assert!(
            run.project.files[1]
                .source
                .contains("export const two: number = 2")
        );
    }

    #[test]
    fn persisted_bun_bundle_split_does_not_reextract_original_bundle_source() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/entrypoints/cli.js",
            Some(
                "// @bun @bytecode @bun-cjs\n(function(exports, require, module, __filename, __dirname) {\nvar o=(H,$)=>()=>($||H(($={exports:{}}).exports,$),$.exports);\nvar pkg=o((ex,m)=>{ex.answer=42;});\n})"
                    .to_string(),
            ),
        ));
        rows.source_files.push(SourceFileInput::new(
            2,
            "__reverts_synthetic__/source-2.js",
            Some(
                "var o=(H,$)=>()=>($||H(($={exports:{}}).exports,$),$.exports);\nvar pkg=o((ex,m)=>{ex.answer=42;});"
                    .to_string(),
            ),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "esbuild:pkg", "modules/pkg.ts")
                .with_source_file(2)
                .with_source_span(SourceSpan::new(0, 98)),
        );
        let input = InputBundle::from_rows(rows).expect("persisted bundle rows should be valid");

        let missing = super::missing_module_source_file_ids(&input);

        assert!(
            missing.is_empty(),
            "original Bun bundle source must not be re-extracted once synthetic esbuild modules are persisted: {missing:?}"
        );
    }

    #[test]
    fn shared_pipeline_preparation_extracts_bundle_modules_once() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some(
                r#"var __commonJS=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports);
var inner = __commonJS({"src/inner.js": (exports, module) => { exports.answer = 42; }});"#
                    .to_string(),
            ),
        ));

        let prepared = prepare_input_rows_for_pipeline(rows);

        assert!(prepared.audit.is_clean());
        assert_eq!(prepared.synthetic_modules.len(), 1);
        assert_eq!(prepared.rows.modules.len(), 1);
        assert_eq!(prepared.rows.modules[0].semantic_path, "src/inner.js");

        let prepared_again = prepare_input_rows_for_pipeline(prepared.rows);
        assert!(
            prepared_again.synthetic_modules.is_empty(),
            "bundle preparation must be idempotent so match and generate can share it"
        );
        assert_eq!(prepared_again.rows.modules.len(), 1);
    }

    #[test]
    fn shared_pipeline_preparation_keeps_enriched_dependency_edges() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some(
                r#"var __commonJS=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports);
var dep = __commonJS({"src/dep.js": (exports, module) => { function shared(){ return 1; } exports.shared = shared; }});
var user = __commonJS({"src/user.js": (exports, module) => { var value = shared(); exports.value = value; }});"#
                    .to_string(),
            ),
        ));

        let prepared = prepare_input_rows_for_pipeline(rows);

        assert!(
            prepared.rows.dependencies.iter().any(|dependency| {
                matches!(dependency.target, ModuleDependencyTarget::Module(_))
                    && dependency.from_module_id
                        != match dependency.target {
                            ModuleDependencyTarget::Module(target) => target,
                            ModuleDependencyTarget::Package { .. } => dependency.from_module_id,
                        }
            }),
            "matcher preparation should expose synthesized module wiring to package/source graph matchers"
        );
    }

    #[test]
    fn prepare_input_bundle_for_generation_is_noop_when_modules_already_attached() {
        // Simulate the post-matcher state: rows reloaded from SQLite where
        // every source file already has at least one module pointing at
        // it (top-level + bundle-extracted inner). The generator-side
        // prep must observe this and skip extraction entirely.
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some(
                r#"var __commonJS=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports);
var inner = __commonJS({"src/inner.js": (exports, module) => { exports.answer = 42; }});"#
                    .to_string(),
            ),
        ));
        let prepared = prepare_input_rows_for_pipeline(rows);
        let input =
            InputBundle::from_rows(prepared.rows).expect("matcher-prepared rows should be valid");
        let module_count_before = input.modules.len();

        let (input_after, audit) =
            prepare_input_bundle_for_generation(input).expect("generation prep should succeed");

        assert!(audit.is_clean());
        assert_eq!(
            input_after.modules.len(),
            module_count_before,
            "second-pass extraction must add no modules"
        );
    }

    #[test]
    fn generate_project_reuses_bundle_preparation_for_inner_modules() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some(
                r#"var __commonJS=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports);
var inner = __commonJS({"src/inner.js": (exports, module) => { exports.answer = 42; }});"#
                    .to_string(),
            ),
        ));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean(), "{:?}", run.audit.findings());
        assert_eq!(run.project.files.len(), 1);
        assert!(
            run.project.files[0].source.contains("exports.answer = 42"),
            "generated project should use bundle-extracted inner module source, got:\n{}",
            run.project.files[0].source
        );
    }

    #[test]
    fn esbuild_commonjs_factory_calls_import_sibling_factories() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some(
                r#"var p=(q,K)=>()=>(K||q((K={exports:{}}).exports,K),K.exports);
var dep=p((exports,module)=>{module.exports={answer:42};});
var main=p((exports,module)=>{var d=dep();module.exports=d.answer;});
main();"#
                    .to_string(),
            ),
        ));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean(), "{:?}", run.audit.findings());
        let main = run
            .project
            .files
            .iter()
            .find(|file| file.path.contains("esbuild-main"))
            .expect("main factory module should be emitted");
        assert!(
            main.source.contains("import { dep }"),
            "main factory should import the sibling dep factory, got:\n{}",
            main.source
        );
        assert!(
            run.project
                .files
                .iter()
                .any(|file| file.path.contains("esbuild-dep")),
            "dep factory module should be emitted"
        );
    }

    #[test]
    fn source_module_symbol_hint_not_recovered_from_ast_is_not_synthesized() {
        let mut rows = rows_with_application_source("export const real = 1;");
        rows.symbols.push(SymbolInput::new(ModuleId(1), "missing"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        assert_eq!(run.project.files.len(), 1);
        assert!(!run.project.files[0].source.contains("missing"));
    }

    #[test]
    fn unresolved_ast_read_is_reported_as_warning() {
        let rows = rows_with_application_source("missing();");
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should return audit");

        // Per ADR 0002 a missing binding read is an input condition: the
        // audit names it as a warning and emission proceeds. The emitted
        // file references `missing` and would fail at runtime/type-check;
        // the audit is loud enough for the consumer to fix the slice.
        let finding = run
            .audit
            .findings()
            .iter()
            .find(|finding| {
                finding.code == FindingCode::MissingDefinition
                    && finding.binding.as_deref() == Some("missing")
            })
            .expect("missing-binding finding present");
        assert_eq!(finding.severity, Severity::Warning);
    }

    #[test]
    fn synthesis_audit_accepts_declared_export_and_rejects_missing_export_binding() {
        let mut file = PlannedFile::new("src/index.ts");
        file.add_binding(PlannedBinding::new(
            BindingName::new("real"),
            BindingName::new("real"),
            BindingShape::Unknown,
            true,
        ));
        file.add_export(BindingName::new("real"));
        file.add_export(BindingName::new("missing"));
        let mut plan = EmitPlan::default();
        plan.push_file(file);

        let audit = audit_emit_plan_synthesis(&plan);

        assert_eq!(audit.findings().len(), 1);
        assert_eq!(
            audit.findings()[0].code,
            FindingCode::SyntheticReferenceWithoutDeclaration
        );
        assert_eq!(audit.findings()[0].binding.as_deref(), Some("missing"));
    }

    #[test]
    fn audit_warns_when_binding_written_from_chained_call_is_member_read_unguarded() {
        // Faithful decompilation of the Claude Code 2.0.75 pattern at uH0/
        // CT2: `X = (await fetch(...)).data.value;` followed by `X.foo` with
        // no null guard. The unguarded read crashes at runtime when the
        // chain resolves to null. ADR 0002 forbids repairing the input — we
        // surface the input bug as an audit warning instead.
        let source = concat!(
            "var X;\n",
            "async function setUp() {\n",
            "    X = (await fetch('/x')).data.value;\n",
            "}\n",
            "function read() {\n",
            "    return X.foo;\n",
            "}\n",
            "setUp();\n",
            "read();\n",
        );
        let rows = rows_with_application_source(source);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should run");

        let finding = run
            .audit
            .findings()
            .iter()
            .find(|finding| finding.code == FindingCode::UnprotectedNullableMemberRead)
            .expect("UnprotectedNullableMemberRead finding for X");
        assert_eq!(finding.binding.as_deref(), Some("X"));
        assert!(
            finding.message.contains("X"),
            "finding message should name the binding; got {:?}",
            finding.message,
        );
    }

    #[test]
    fn audit_warns_when_nullable_binding_aliased_at_module_scope_is_member_read() {
        // Alias-closure exercise: X is written from a nullable chain, then
        // a module-scope alias A = X picks it up, then A.foo is read. The
        // direct `X.foo` doesn't exist, so the audit must walk the alias
        // closure on the read target (A → X) to find the maybe-nullable
        // source X.
        //
        // The p1 `CT2 / UT2() / uH0` pattern uses a function-local alias
        // (`let A = getX()` inside `read()`); locals are currently
        // outside the fact extractor's scope filter (to avoid name
        // collisions across nested functions), so that exact case is not
        // yet caught. Scope-qualified binding identity is the natural
        // follow-up.
        let source = concat!(
            "var X;\n",
            "var A;\n",
            "async function setUp() {\n",
            "    X = (await fetch('/x')).data.value;\n",
            "}\n",
            "function init() {\n",
            "    A = X;\n",
            "    return A.foo;\n",
            "}\n",
            "setUp();\n",
            "init();\n",
        );
        let rows = rows_with_application_source(source);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should run");

        let finding = run
            .audit
            .findings()
            .iter()
            .find(|finding| finding.code == FindingCode::UnprotectedNullableMemberRead)
            .expect("audit must fire on X via the alias closure A → X");
        assert_eq!(finding.binding.as_deref(), Some("X"));
    }

    #[test]
    fn audit_does_not_warn_when_only_object_literal_writes_target_binding() {
        // Negative control: a binding only assigned from object literals
        // (no chained call/await on the RHS) must NOT trip the audit.
        let source = concat!(
            "var X = {};\n",
            "function reset() { X = { ready: true }; }\n",
            "function read() { return X.ready; }\n",
            "reset();\n",
            "read();\n",
        );
        let rows = rows_with_application_source(source);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should run");

        assert!(
            !run.audit.has(FindingCode::UnprotectedNullableMemberRead),
            "object-literal writes must not trigger the audit; got: {:?}",
            run.audit.findings(),
        );
    }

    #[test]
    fn namespace_object_member_audit_fires_when_emit_drops_a_known_member() {
        // Paper #7 downstream consumer: when the planner recorded that `ns`
        // is accessed via `.foo` and `.bar`, but the emitted source only
        // mentions `ns.foo`, the audit must call this out so a refactor that
        // accidentally strips a member never ships silently.
        let mut file = PlannedFile::new("src/index.ts");
        let mut known = BTreeSet::new();
        known.insert(BindingName::new("foo"));
        known.insert(BindingName::new("bar"));
        file.add_binding(
            PlannedBinding::new(
                BindingName::new("ns"),
                BindingName::new("ns"),
                BindingShape::NamespaceObject,
                true,
            )
            .with_known_members(known),
        );
        let mut plan = EmitPlan::default();
        plan.push_file(file);

        let project = EmittedProject {
            files: vec![EmittedFile {
                path: "src/index.ts".to_string(),
                source: "const ns = { foo: 1, bar: 2 };\nconst a = ns.foo;\n".to_string(),
            }],
        };

        let audit = audit_namespace_object_member_consistency(&plan, &project);

        assert_eq!(audit.findings().len(), 1);
        let finding = &audit.findings()[0];
        assert_eq!(finding.code, FindingCode::NamespaceMemberStripped);
        assert_eq!(finding.binding.as_deref(), Some("ns"));
        assert!(
            finding.message.contains("bar"),
            "finding must name the stripped member; got {:?}",
            finding.message,
        );
        assert!(
            !finding.message.contains("foo"),
            "finding must not blame members that are still present; got {:?}",
            finding.message,
        );
    }

    #[test]
    fn namespace_object_member_audit_is_silent_when_emit_keeps_all_members() {
        let mut file = PlannedFile::new("src/index.ts");
        let mut known = BTreeSet::new();
        known.insert(BindingName::new("foo"));
        known.insert(BindingName::new("bar"));
        file.add_binding(
            PlannedBinding::new(
                BindingName::new("ns"),
                BindingName::new("ns"),
                BindingShape::NamespaceObject,
                true,
            )
            .with_known_members(known),
        );
        let mut plan = EmitPlan::default();
        plan.push_file(file);

        let project = EmittedProject {
            files: vec![EmittedFile {
                path: "src/index.ts".to_string(),
                source: "const ns = { foo: 1, bar: 2 };\nns.foo;\nns.bar;".to_string(),
            }],
        };

        let audit = audit_namespace_object_member_consistency(&plan, &project);
        assert!(audit.findings().is_empty());
    }

    #[test]
    fn namespace_object_member_audit_accepts_quoted_property_access() {
        // esbuild and similar tools emit `ns["mustBeQuoted"]` for names that
        // are not valid bare identifiers. The audit must treat that as a real
        // access, not a missing member — otherwise it false-fires on every
        // bundle that quotes a property.
        let mut file = PlannedFile::new("src/index.ts");
        let mut known = BTreeSet::new();
        known.insert(BindingName::new("plain"));
        known.insert(BindingName::new("must-be-quoted"));
        known.insert(BindingName::new("with space"));
        file.add_binding(
            PlannedBinding::new(
                BindingName::new("ns"),
                BindingName::new("ns"),
                BindingShape::NamespaceObject,
                true,
            )
            .with_known_members(known),
        );
        let mut plan = EmitPlan::default();
        plan.push_file(file);

        let project = EmittedProject {
            files: vec![EmittedFile {
                path: "src/index.ts".to_string(),
                source: r#"ns.plain; ns["must-be-quoted"]; ns['with space'];"#.to_string(),
            }],
        };

        let audit = audit_namespace_object_member_consistency(&plan, &project);
        assert!(
            audit.findings().is_empty(),
            "expected no findings, got: {:?}",
            audit.findings(),
        );
    }

    #[test]
    fn namespace_object_member_audit_accepts_object_destructuring() {
        let mut file = PlannedFile::new("src/index.ts");
        let mut known = BTreeSet::new();
        known.insert(BindingName::new("createClient"));
        known.insert(BindingName::new("close"));
        file.add_binding(
            PlannedBinding::new(
                BindingName::new("api"),
                BindingName::new("api"),
                BindingShape::NamespaceObject,
                true,
            )
            .with_known_members(known),
        );
        let mut plan = EmitPlan::default();
        plan.push_file(file);

        let project = EmittedProject {
            files: vec![EmittedFile {
                path: "src/index.ts".to_string(),
                source: "const { createClient: client, close } = api;\nclient(); close();"
                    .to_string(),
            }],
        };

        let audit = audit_namespace_object_member_consistency(&plan, &project);
        assert!(
            audit.findings().is_empty(),
            "expected no findings, got: {:?}",
            audit.findings(),
        );
    }

    #[test]
    fn namespace_object_member_audit_ignores_non_namespace_shapes() {
        // Non-NamespaceObject bindings must never be inspected, even if they
        // somehow carry known_members — the planner shouldn't attach them
        // there, but the audit's gate is the shape, not the field.
        let mut file = PlannedFile::new("src/index.ts");
        let mut known = BTreeSet::new();
        known.insert(BindingName::new("never_emitted"));
        file.add_binding(
            PlannedBinding::new(
                BindingName::new("not_a_namespace"),
                BindingName::new("not_a_namespace"),
                BindingShape::Callable,
                true,
            )
            .with_known_members(known),
        );
        let mut plan = EmitPlan::default();
        plan.push_file(file);

        let project = EmittedProject {
            files: vec![EmittedFile {
                path: "src/index.ts".to_string(),
                source: "function not_a_namespace() {}\n".to_string(),
            }],
        };

        let audit = audit_namespace_object_member_consistency(&plan, &project);
        assert!(audit.findings().is_empty());
    }

    #[test]
    fn callable_shape_emitted_as_value_declaration_is_reported_by_pipeline() {
        // The bundle defines `runner` as a plain value but also calls it. The shape
        // solver upgrades `runner` to Callable; the audit must notice that the
        // emitted source declares `runner` as a non-callable variable and report.
        let source = "const runner = 42;\nrunner();\n";
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/runner.ts",
            Some(source.to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "runner", "src/runner.ts").with_source_file(1),
        );
        rows.symbols.push(SymbolInput::new(ModuleId(1), "runner"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        let finding = run
            .audit
            .findings()
            .iter()
            .find(|finding| finding.code == FindingCode::CallableEmittedAsNonCallable)
            .expect("expected CallableEmittedAsNonCallable finding");
        assert!(
            finding.message.contains("source-backed"),
            "finding must blame the source, not the emitter; got: {:?}",
            finding.message,
        );
        assert!(
            finding.message.contains("called like a function"),
            "finding must describe the runtime-error symptom; got: {:?}",
            finding.message,
        );
    }

    #[test]
    fn enum_iife_source_solves_to_enum_object_shape_and_preserves_body_in_output() {
        let enum_source = "var Color;\n\
            (function (Color) {\n\
                Color[Color[\"Red\"] = 0] = \"Red\";\n\
                Color[Color[\"Green\"] = 1] = \"Green\";\n\
            })(Color || (Color = {}));\n";
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/colors.ts",
            Some(enum_source.to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "colors", "src/colors.ts").with_source_file(1),
        );
        rows.symbols.push(SymbolInput::new(ModuleId(1), "Color"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let model = ProgramModel::from_input(input.clone());
        let enrichment = enrich_program(model);
        assert_eq!(
            enrichment.program.binding_shape(ModuleId(1), "Color"),
            BindingShape::EnumObject,
            "enum-IIFE source must solve to EnumObject shape",
        );

        let run = generate_project_from_input(input).expect("fixture should emit");
        assert!(
            run.audit.is_clean(),
            "expected clean audit, got: {:?}",
            run.audit.findings(),
        );
        assert_eq!(run.project.files.len(), 1);
        let emitted = run.project.files[0].source.as_str();
        assert!(
            emitted.contains("Color[Color['Red'] = 0]"),
            "emitted source must preserve enum reverse-mapping body, got:\n{emitted}",
        );
        assert!(
            emitted.contains("Color[Color['Green'] = 1]"),
            "emitted source must preserve all enum members, got:\n{emitted}",
        );
    }

    #[test]
    fn webpack_bundle_emits_compiler_specific_preservation_banner() {
        // The fixture has the canonical __webpack_require__ identifier at module
        // scope, so compiler detection classifies the module as Webpack and the
        // pipeline must surface that decision via a preservation banner.
        let source = "var __webpack_require__ = function (id) { return id; };\n\
                      var entry = __webpack_require__(1);\n";
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/runtime.ts",
            Some(source.to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "runtime", "src/runtime.ts").with_source_file(1),
        );
        rows.symbols
            .push(SymbolInput::new(ModuleId(1), "__webpack_require__"));
        rows.symbols.push(SymbolInput::new(ModuleId(1), "entry"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");
        assert!(
            run.audit.is_clean(),
            "expected clean audit, got: {:?}",
            run.audit.findings(),
        );
        let emitted = run.project.files[0].source.as_str();
        assert!(
            emitted.contains("// reverts-compiler-preserved: webpack"),
            "emitted source must carry a webpack preservation banner, got:\n{emitted}",
        );
    }

    #[test]
    fn webpack_runtime_identifier_inside_function_does_not_classify_as_webpack() {
        // Runtime helper names inside function bodies are filtered out by the
        // module-scope AST fact rule. Compiler detection must not classify
        // those bundles from raw text alone.
        let source = "function activate() {\n  __webpack_require__(1);\n  return 42;\n}\n";
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/runtime.ts",
            Some(source.to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "runtime", "src/runtime.ts").with_source_file(1),
        );
        rows.symbols.push(SymbolInput::new(ModuleId(1), "activate"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");
        assert!(
            run.audit.is_clean(),
            "expected clean audit, got: {:?}",
            run.audit.findings(),
        );
        let emitted = run.project.files[0].source.as_str();
        assert!(
            !emitted.contains("// reverts-compiler-preserved: webpack"),
            "in-function webpack runtime identifier must not trigger webpack banner, got:\n{emitted}",
        );
    }

    #[test]
    fn unknown_compiler_does_not_emit_preservation_banner() {
        // Plain TypeScript source with no bundler signals must NOT carry a
        // preservation banner; the banner is reserved for non-Unknown compilers.
        let source = "export function add(a: number, b: number) {\n  return a + b;\n}\n";
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/add.ts",
            Some(source.to_string()),
        ));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "add", "src/add.ts").with_source_file(1));
        rows.symbols.push(SymbolInput::new(ModuleId(1), "add"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");
        let emitted = run.project.files[0].source.as_str();
        assert!(
            !emitted.contains("reverts-compiler-preserved"),
            "unknown-compiler module must not include a preservation banner, got:\n{emitted}",
        );
    }

    fn run_with_source(source: &str, symbols: &[&str]) -> OutputRun {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/bundle.ts",
            Some(source.to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "bundle", "src/bundle.ts").with_source_file(1),
        );
        for symbol in symbols {
            rows.symbols.push(SymbolInput::new(ModuleId(1), *symbol));
        }
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        generate_project_from_input(input).expect("fixture should emit")
    }

    #[test]
    fn esbuild_runtime_identifier_in_function_does_not_emit_esbuild_banner() {
        let source = "function setup() {\n  __toCommonJS({});\n}\n";
        let run = run_with_source(source, &["setup"]);
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            !emitted.contains("// reverts-compiler-preserved: esbuild"),
            "function-local esbuild identifier must not trigger esbuild banner, got:\n{emitted}",
        );
    }

    #[test]
    fn symbol_index_maps_emitted_bindings_to_files() {
        let source = "export function parse(a){ return a + 1; }\n";
        let run = run_with_source(source, &["parse"]);
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let entry = run
            .symbol_index
            .iter()
            .find(|entry| entry.original_name == "parse")
            .expect("parse should be indexed");
        // The indexed file must be one of the actually emitted files.
        assert!(
            run.project
                .files
                .iter()
                .any(|file| file.path == entry.file_path),
            "indexed file {} not among emitted files",
            entry.file_path
        );
    }

    #[test]
    fn symbol_index_keeps_source_backed_unnamed_bindings_actionable() {
        let mut rows = rows_with_application_source("var pendingName = 1;");
        rows.symbols
            .push(SymbolInput::new(ModuleId(1), "pendingName"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let program = super::prepare_and_enrich(input)
            .expect("fixture should enrich")
            .program;
        let mut module_paths = BTreeMap::new();
        module_paths.insert(ModuleId(1), "modules/1.ts".to_string());
        let emitted = EmittedProject {
            files: vec![EmittedFile {
                path: "modules/1.ts".to_string(),
                source: "var pendingName = 1;".to_string(),
            }],
        };

        let entries = super::build_symbol_index(
            &program,
            &module_paths,
            &emitted,
            &std::collections::BTreeSet::new(),
            &[],
            &std::collections::BTreeSet::new(),
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].original_name, "pendingName");
        assert_eq!(entries[0].emitted_name, "pendingName");
        assert!(!entries[0].semantic_named);
    }

    #[test]
    fn symbol_index_drops_emitted_only_fallback_for_accepted_symbols() {
        let mut rows = rows_with_application_source("var value = 1;");
        rows.symbols
            .push(SymbolInput::new(ModuleId(1), "value").with_semantic_name("exportValue"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let program = super::prepare_and_enrich(input)
            .expect("fixture should enrich")
            .program;
        let mut module_paths = BTreeMap::new();
        module_paths.insert(ModuleId(1), "modules/1.ts".to_string());
        let emitted = EmittedProject {
            files: vec![EmittedFile {
                path: "modules/1.ts".to_string(),
                source: "var value = 1;".to_string(),
            }],
        };

        let entries = super::build_symbol_index(
            &program,
            &module_paths,
            &emitted,
            &std::collections::BTreeSet::new(),
            &[],
            &std::collections::BTreeSet::new(),
        );

        assert!(
            entries.is_empty(),
            "emitted-only name `value` must not be reported as an actionable fallback: {entries:?}"
        );
    }

    #[test]
    fn symbol_index_registers_unmodularized_recovered_code_file_bindings() {
        // Failure mode: a scope-hoisting bundler's eager entry island is
        // emitted as one synthesized file owned by no model module. Without the
        // planner marker, its top-level declarations were silently absent from
        // the symbol index — the naming denominator excluded virtually the
        // whole application while reporting itself complete.
        let rows = rows_with_application_source("var x = 1;");
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let program = super::prepare_and_enrich(input)
            .expect("fixture should enrich")
            .program;
        let mut module_paths = BTreeMap::new();
        module_paths.insert(ModuleId(1), "modules/1.ts".to_string());
        let island_source =
            "var qT = 1;\nfunction Fv() { return qT; }\nvar dead0;\nexport { Fv };\n";
        let emitted = EmittedProject {
            files: vec![
                EmittedFile {
                    path: "modules/1.ts".to_string(),
                    source: "var x = 1;".to_string(),
                },
                EmittedFile {
                    path: "modules/entry-island.ts".to_string(),
                    source: island_source.to_string(),
                },
            ],
        };
        let marked = std::collections::BTreeSet::from(["modules/entry-island.ts".to_string()]);

        let entries = super::build_symbol_index(
            &program,
            &module_paths,
            &emitted,
            &marked,
            &[],
            &std::collections::BTreeSet::new(),
        );

        let island_entries: Vec<_> = entries
            .iter()
            .filter(|entry| entry.file_path == "modules/entry-island.ts")
            .collect();
        assert!(
            island_entries.iter().all(|entry| entry.module_id.is_none()),
            "island bindings own no module id: {island_entries:?}"
        );
        let exported_fn = island_entries
            .iter()
            .find(|entry| entry.original_name == "Fv")
            .expect("island function must be indexed");
        assert!(exported_fn.function_like);
        assert!(
            exported_fn.exported,
            "export {{ Fv }} must mark the binding"
        );
        assert!(!exported_fn.semantic_named);
        let value = island_entries
            .iter()
            .find(|entry| entry.original_name == "qT")
            .expect("island value must be indexed");
        assert!(!value.exported);
        assert!(!value.dead, "qT is read by Fv");
        let dead = island_entries
            .iter()
            .find(|entry| entry.original_name == "dead0")
            .expect("dead island binding still indexed, flagged dead");
        assert!(dead.dead);

        // The same unowned file WITHOUT the marker stays excluded: scaffold and
        // runtime-glue files are not naming work.
        let unmarked = super::build_symbol_index(
            &program,
            &module_paths,
            &emitted,
            &std::collections::BTreeSet::new(),
            &[],
            &std::collections::BTreeSet::new(),
        );
        assert!(
            unmarked
                .iter()
                .all(|entry| entry.file_path != "modules/entry-island.ts"),
            "unmarked unowned files must stay out of the symbol index"
        );
    }

    #[test]
    fn symbol_index_drops_package_anchored_island_bindings() {
        // Failure mode: a bundled library (zod, etc.) is flattened into the eager
        // island. Its functions are not application naming work, yet they sit in
        // the island file and would inflate the naming denominator. When the
        // package matcher anchors such a binding to a library, it must leave the
        // symbol index entirely — while a genuine, unanchored island binding (and
        // any same-named binding in a real module) stays.
        let rows = rows_with_application_source("var x = 1;");
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let program = super::prepare_and_enrich(input)
            .expect("fixture should enrich")
            .program;
        let mut module_paths = BTreeMap::new();
        module_paths.insert(ModuleId(1), "modules/1.ts".to_string());
        let emitted = EmittedProject {
            files: vec![
                // A real module that happens to share the anchored minified name.
                EmittedFile {
                    path: "modules/1.ts".to_string(),
                    source: "function Fv() { return 1; }\nexport { Fv };\n".to_string(),
                },
                EmittedFile {
                    path: "modules/entry-island.ts".to_string(),
                    source: "function Fv() { return 2; }\nvar appState = 3;\nexport { Fv };\n"
                        .to_string(),
                },
            ],
        };
        let marked = std::collections::BTreeSet::from(["modules/entry-island.ts".to_string()]);
        let anchored = std::collections::BTreeSet::from(["Fv".to_string()]);

        let entries =
            super::build_symbol_index(&program, &module_paths, &emitted, &marked, &[], &anchored);

        // The island's anchored `Fv` is gone from the denominator.
        assert!(
            !entries.iter().any(|entry| entry.module_id.is_none()
                && entry.file_path == "modules/entry-island.ts"
                && entry.original_name == "Fv"),
            "package-anchored island binding must be dropped: {entries:?}"
        );
        // The genuine island binding stays.
        assert!(
            entries
                .iter()
                .any(|entry| entry.module_id.is_none() && entry.original_name == "appState"),
            "unanchored island binding must remain: {entries:?}"
        );
        // The same-named binding in a real module is untouched (gated on module_id).
        assert!(
            entries
                .iter()
                .any(|entry| entry.module_id == Some(ModuleId(1)) && entry.original_name == "Fv"),
            "a real module's same-named binding must not be dropped: {entries:?}"
        );
    }

    #[test]
    fn symbol_index_maps_renamed_island_binding_back_to_original_key() {
        // After `binding-names --accept`, regeneration emits the island with the
        // semantic name applied. The index must key the entry by its ORIGINAL
        // minified name (so the DB key space stays stable) and count it as
        // semantically named — otherwise every accepted island rename would
        // re-enter the worklist as a fresh unnamed binding.
        let rows = rows_with_application_source("var x = 1;");
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let program = super::prepare_and_enrich(input)
            .expect("fixture should enrich")
            .program;
        let module_paths = BTreeMap::new();
        let emitted = EmittedProject {
            files: vec![EmittedFile {
                path: "modules/entry-island.ts".to_string(),
                source: "var configRegistry = 1;\nexport { configRegistry };\n".to_string(),
            }],
        };
        let marked = std::collections::BTreeSet::from(["modules/entry-island.ts".to_string()]);
        let renames = vec![LocalBindingRename {
            file_path: "modules/entry-island.ts".to_string(),
            original_name: "qT".to_string(),
            binding_index: None,
            semantic_name: "configRegistry".to_string(),
        }];

        let entries = super::build_symbol_index(
            &program,
            &module_paths,
            &emitted,
            &marked,
            &renames,
            &std::collections::BTreeSet::new(),
        );

        let entry = entries
            .iter()
            .find(|entry| entry.emitted_name == "configRegistry")
            .expect("renamed island binding must be indexed");
        assert_eq!(entry.original_name, "qT");
        assert!(entry.semantic_named);
        assert!(entry.exported);
        assert!(entry.module_id.is_none());
    }

    #[test]
    fn generated_island_bindings_enter_symbol_index_end_to_end() {
        // Full pipeline run on a bundle whose runtime entrypoint pulls eager
        // top-level code into the planner's entrypoint island. The island file
        // must surface in `unmodularized_code_paths` and its declarations must
        // land in the symbol index with `module_id: None`.
        let prelude = "function main() { return helper(); }\nfunction helper() { return 1; }\n";
        let body = "var cliEntry = () => 'ok';\n";
        let tail = "main();\n";
        let source = format!("{prelude}{body}{tail}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + body.len()) as u32,
                )),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        let island_path = run
            .unmodularized_code_paths
            .iter()
            .next()
            .expect("the eager entry code should be planned as a marked island file");
        let island_entries: Vec<_> = run
            .symbol_index
            .iter()
            .filter(|entry| &entry.file_path == island_path)
            .collect();
        assert!(
            island_entries
                .iter()
                .any(|entry| entry.original_name == "main"),
            "island entry function must be indexed for naming: {island_entries:?}"
        );
        assert!(
            island_entries.iter().all(|entry| entry.module_id.is_none()),
            "island bindings own no module id: {island_entries:?}"
        );
    }

    #[test]
    fn rollup_object_freeze_pattern_alone_does_not_emit_rollup_banner() {
        let source = "var frozen = Object.freeze({ answer: 42 });\n";
        let run = run_with_source(source, &["frozen"]);
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            !emitted.contains("// reverts-compiler-preserved: rollup"),
            "Object.freeze alone must not trigger rollup banner, got:\n{emitted}",
        );
    }

    #[test]
    fn babel_interop_helper_in_function_does_not_emit_babel_banner() {
        let source = "function load(mod) {\n  return _interopRequireDefault(mod);\n}\n";
        let run = run_with_source(source, &["load"]);
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            !emitted.contains("// reverts-compiler-preserved: babel"),
            "function-local babel identifier must not trigger babel banner, got:\n{emitted}",
        );
    }

    #[test]
    fn babel_es_module_marker_is_stripped_from_emitted_output() {
        // The `Object.defineProperty(exports, "__esModule", { value: true })`
        // statement is a no-op in ESM output. When the module is classified
        // as Babel, the lowering pipeline must strip it from the emit.
        let source = "Object.defineProperty(exports, \"__esModule\", { value: true });\n\
                      function _interopRequireDefault(e) { return e && e.__esModule ? e : { default: e }; }\n\
                      function load(mod) {\n  return _interopRequireDefault(mod);\n}\n";
        let run = run_with_source(source, &["load"]);
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            emitted.contains("// reverts-compiler-preserved: babel"),
            "babel fixture must keep its banner, got:\n{emitted}",
        );
        assert!(
            !emitted.contains("Object.defineProperty(exports, \"__esModule\""),
            "babel lowering must strip the __esModule marker, got:\n{emitted}",
        );
        assert!(
            emitted.contains("function load"),
            "babel lowering must keep unrelated declarations, got:\n{emitted}",
        );
    }

    #[test]
    fn babel_interop_require_default_call_is_rewritten_to_default_wrapped_require() {
        // Babel's classic CJS interop pattern wraps a require with the
        // `_interopRequireDefault` helper:
        //     var _foo = _interopRequireDefault(require("./foo"));
        // After lowering the helper call is dropped and the original require
        // is wrapped in a plain `{ default: ... }` literal — that preserves
        // every existing `_foo.default` access without keeping the helper
        // around.
        let source = "var _foo = _interopRequireDefault(require('./foo'));\n\
                      function _interopRequireDefault(e) { return e && e.__esModule ? e : { default: e }; }\n\
                      var entry = _foo.default;\n";
        let run = run_with_source(source, &["_foo", "_interopRequireDefault", "entry"]);
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            emitted.contains("// reverts-compiler-preserved: babel"),
            "babel fixture must keep its banner; got:\n{emitted}",
        );
        assert!(
            emitted.contains("var _foo = { default: require('./foo') }"),
            "babel lowering must rewrite the helper call into a literal default wrapper; got:\n{emitted}",
        );
        assert!(
            !emitted.contains("_interopRequireDefault(require("),
            "babel lowering must drop the helper *call* (its definition may remain unused); got:\n{emitted}",
        );
        assert!(
            emitted.contains("var entry = _foo.default"),
            "babel lowering must preserve subsequent .default accesses; got:\n{emitted}",
        );
        assert!(
            !emitted.contains("function _interopRequireDefault"),
            "babel lowering must strip the helper definition once all calls are rewritten; got:\n{emitted}",
        );
    }

    #[test]
    fn babel_interop_require_wildcard_call_is_rewritten_to_bare_require() {
        // Babel's `_interopRequireWildcard(require("X"))` wraps a CJS namespace
        // import. Lowering it to a bare `require("X")` is safe for ESM and
        // CJS-with-named-exports — the common cases babel targets. The helper
        // definition becomes dead and must be stripped.
        let source = "var foo = _interopRequireWildcard(require('./foo'), true);\n\
                      function _interopRequireWildcard(e, t) { return e && e.__esModule ? e : { default: e }; }\n\
                      var bar = foo.bar;\n";
        let run = run_with_source(source, &["foo", "_interopRequireWildcard", "bar"]);
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            emitted.contains("var foo = require('./foo')"),
            "wildcard call must be lowered to bare require; got:\n{emitted}",
        );
        assert!(
            !emitted.contains("_interopRequireWildcard(require("),
            "wildcard helper call must be removed; got:\n{emitted}",
        );
        assert!(
            !emitted.contains("function _interopRequireWildcard"),
            "wildcard helper definition must be stripped once dead; got:\n{emitted}",
        );
        assert!(
            emitted.contains("var bar = foo.bar"),
            "downstream member access must be preserved; got:\n{emitted}",
        );
    }

    #[test]
    fn babel_interop_require_wildcard_helper_is_kept_when_a_call_remains_unrewritten() {
        // If a wildcard call cannot be rewritten (e.g. a non-var-init usage)
        // the helper must stay alive — same protective invariant as for the
        // _interopRequireDefault helper.
        let source = "var foo = _interopRequireWildcard(require('./foo'));\n\
                      function _interopRequireWildcard(e) { return e && e.__esModule ? e : { default: e }; }\n\
                      var bar = _interopRequireWildcard({});\n\
                      var entry = foo.entry;\n\
                      var inert = bar.default;\n";
        let run = run_with_source(
            source,
            &["foo", "_interopRequireWildcard", "bar", "entry", "inert"],
        );
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            emitted.contains("function _interopRequireWildcard"),
            "wildcard helper definition must remain because a call survives; got:\n{emitted}",
        );
        assert!(
            emitted.contains("_interopRequireWildcard({})"),
            "untouched wildcard call must be present; got:\n{emitted}",
        );
    }

    #[test]
    fn babel_interop_require_default_helper_is_kept_when_a_call_remains_unrewritten() {
        // The helper definition may only be stripped when nothing in the
        // emitted output still references it. A non-rewritable use (e.g.
        // a top-level call without the var-init pattern) must keep the
        // helper alive so the runtime stays valid.
        let source = "var _foo = _interopRequireDefault(require('./foo'));\n\
                      function _interopRequireDefault(e) { return e && e.__esModule ? e : { default: e }; }\n\
                      var _bar = _interopRequireDefault({});\n\
                      var entry = _foo.default;\n\
                      var bar = _bar.default;\n";
        let run = run_with_source(
            source,
            &["_foo", "_interopRequireDefault", "_bar", "entry", "bar"],
        );
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            emitted.contains("function _interopRequireDefault"),
            "helper definition must remain because at least one call site survives; got:\n{emitted}",
        );
        assert!(
            emitted.contains("_interopRequireDefault({})"),
            "untouched call must still be present; got:\n{emitted}",
        );
    }

    #[test]
    fn esbuild_runtime_helper_definitions_are_stripped_when_unused() {
        // Esbuild bundles always declare a fixed set of CJS-interop helpers
        // at the top (`__commonJS`, `__toCommonJS`, `__defProp`, ...). When
        // none of them are referenced in the emitted module body, the
        // lowering must strip the dead definitions.
        let source = "var __commonJS = (cb, mod) => function __require() { return mod; };\n\
                      var __toCommonJS = (mod) => mod;\n\
                      var __defProp = Object.defineProperty;\n\
                      var __export = (target, all) => { return target; };\n\
                      var entry = 42;\n";
        let run = run_with_source(
            source,
            &[
                "__commonJS",
                "__toCommonJS",
                "__defProp",
                "__export",
                "entry",
            ],
        );
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            emitted.contains("// reverts-compiler-preserved: esbuild"),
            "esbuild fixture must carry esbuild banner; got:\n{emitted}",
        );
        for helper in ["__commonJS", "__toCommonJS", "__defProp", "__export"] {
            assert!(
                !emitted.contains(&format!("var {helper} =")),
                "dead esbuild helper `{helper}` definition must be stripped; got:\n{emitted}",
            );
        }
        assert!(
            emitted.contains("var entry: number = 42"),
            "unrelated declarations must be preserved; got:\n{emitted}",
        );
    }

    #[test]
    fn esbuild_runtime_helper_definitions_are_kept_when_referenced() {
        // The strip must only fire when the helper is genuinely unused.
        // Add a call site for `__commonJS` and verify both the helper
        // definition AND its caller survive.
        let source = "var __commonJS = (cb, mod) => function __require() { return mod; };\n\
                      var __toCommonJS = (mod) => mod;\n\
                      var require_foo = __commonJS({ 'foo'() { return 1; } });\n\
                      var entry = require_foo();\n";
        let run = run_with_source(
            source,
            &["__commonJS", "__toCommonJS", "require_foo", "entry"],
        );
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            emitted.contains("// reverts-compiler-preserved: esbuild"),
            "esbuild fixture must carry esbuild banner; got:\n{emitted}",
        );
        assert!(
            emitted.contains("var __commonJS"),
            "referenced helper must remain; got:\n{emitted}",
        );
        assert!(
            !emitted.contains("var __toCommonJS"),
            "unreferenced helper `__toCommonJS` must still be stripped; got:\n{emitted}",
        );
        assert!(
            emitted.contains("var require_foo = __commonJS("),
            "call site must be preserved; got:\n{emitted}",
        );
    }

    #[test]
    fn webpack_runtime_helper_definitions_are_stripped_when_unused() {
        // Webpack 5 typically wraps its runtime helpers inside a top-level
        // IIFE; the no-IIFE case (`output.iife: false`) emits the helpers at
        // module scope where this strip applies directly. The pass is the
        // same `if-unreferenced` pattern used for esbuild.
        let source = "var __webpack_modules__ = {};\n\
                      var __webpack_module_cache__ = {};\n\
                      function __webpack_require__(id) { return id; }\n\
                      var entry = 42;\n";
        let run = run_with_source(
            source,
            &[
                "__webpack_modules__",
                "__webpack_module_cache__",
                "__webpack_require__",
                "entry",
            ],
        );
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            emitted.contains("// reverts-compiler-preserved: webpack"),
            "webpack fixture must carry webpack banner; got:\n{emitted}",
        );
        for helper in [
            "__webpack_modules__",
            "__webpack_module_cache__",
            "__webpack_require__",
        ] {
            let pattern_a = format!("var {helper}");
            let pattern_b = format!("function {helper}");
            assert!(
                !emitted.contains(&pattern_a) && !emitted.contains(&pattern_b),
                "unreferenced webpack helper `{helper}` must be stripped; got:\n{emitted}",
            );
        }
        assert!(
            emitted.contains("var entry: number = 42"),
            "unrelated declarations must be preserved; got:\n{emitted}",
        );
    }

    #[test]
    fn webpack_runtime_helper_definitions_are_kept_when_referenced() {
        let source = "function __webpack_require__(id) { return id; }\n\
                      var __webpack_modules__ = {};\n\
                      var entry = __webpack_require__(1);\n";
        let run = run_with_source(
            source,
            &["__webpack_require__", "__webpack_modules__", "entry"],
        );
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            emitted.contains("// reverts-compiler-preserved: webpack"),
            "webpack fixture must carry webpack banner; got:\n{emitted}",
        );
        assert!(
            emitted.contains("function __webpack_require__"),
            "referenced helper must remain; got:\n{emitted}",
        );
        assert!(
            !emitted.contains("var __webpack_modules__"),
            "unreferenced helper `__webpack_modules__` must still be stripped; got:\n{emitted}",
        );
        assert!(
            emitted.contains("var entry = __webpack_require__(1)"),
            "call site must be preserved; got:\n{emitted}",
        );
    }

    #[test]
    fn babel_interop_require_default_rewrites_every_call_in_the_same_module() {
        // Real Babel CJS output frequently has several `_interopRequireDefault`
        // call sites per module. The lowering must rewrite every occurrence,
        // not just the first one, and the helper must be stripped once all
        // calls are gone.
        let source = "var _foo = _interopRequireDefault(require('./foo'));\n\
                      var _bar = _interopRequireDefault(require('./bar'));\n\
                      var _baz = _interopRequireDefault(require('./baz'));\n\
                      function _interopRequireDefault(e) { return e && e.__esModule ? e : { default: e }; }\n\
                      var entry = [_foo.default, _bar.default, _baz.default];\n";
        let run = run_with_source(
            source,
            &["_foo", "_bar", "_baz", "_interopRequireDefault", "entry"],
        );
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(emitted.contains("var _foo = { default: require('./foo') }"));
        assert!(emitted.contains("var _bar = { default: require('./bar') }"));
        assert!(emitted.contains("var _baz = { default: require('./baz') }"));
        assert!(
            !emitted.contains("_interopRequireDefault(require("),
            "no helper call should survive after multi-call rewrite; got:\n{emitted}",
        );
        assert!(
            !emitted.contains("function _interopRequireDefault"),
            "helper must be stripped once all three calls are rewritten; got:\n{emitted}",
        );
    }

    #[test]
    fn babel_lowering_handles_default_and_wildcard_helpers_in_one_module() {
        // A module that mixes default + wildcard interop must:
        //   - rewrite the default call into `{ default: require(...) }`,
        //   - rewrite the wildcard call into bare `require(...)`,
        //   - strip BOTH helper definitions once they become dead.
        let source = "var _foo = _interopRequireDefault(require('./foo'));\n\
                      var bar = _interopRequireWildcard(require('./bar'));\n\
                      function _interopRequireDefault(e) { return e && e.__esModule ? e : { default: e }; }\n\
                      function _interopRequireWildcard(e, t) { return e && e.__esModule ? e : { default: e }; }\n\
                      var entry = [_foo.default, bar.named];\n";
        let run = run_with_source(
            source,
            &[
                "_foo",
                "bar",
                "_interopRequireDefault",
                "_interopRequireWildcard",
                "entry",
            ],
        );
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(emitted.contains("var _foo = { default: require('./foo') }"));
        assert!(emitted.contains("var bar = require('./bar')"));
        assert!(!emitted.contains("function _interopRequireDefault"));
        assert!(!emitted.contains("function _interopRequireWildcard"));
        assert!(emitted.contains("var entry = [_foo.default, bar.named]"));
    }

    #[test]
    fn babel_lowering_combines_es_module_marker_strip_and_interop_rewrite() {
        // The `__esModule` marker strip and the interop call rewrite must
        // compose: a single emit should drop the marker AND rewrite the
        // helper call AND strip the helper definition.
        let source = "Object.defineProperty(exports, \"__esModule\", { value: true });\n\
                      exports.default = void 0;\n\
                      var _foo = _interopRequireDefault(require('./foo'));\n\
                      function _interopRequireDefault(e) { return e && e.__esModule ? e : { default: e }; }\n\
                      exports.default = _foo.default;\n";
        let run = run_with_source(source, &["_foo", "_interopRequireDefault"]);
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            !emitted.contains("__esModule"),
            "marker must be stripped; got:\n{emitted}",
        );
        assert!(
            emitted.contains("var _foo = { default: require('./foo') }"),
            "interop call must be rewritten; got:\n{emitted}",
        );
        assert!(
            !emitted.contains("function _interopRequireDefault"),
            "helper must be stripped; got:\n{emitted}",
        );
    }

    #[test]
    fn babel_helper_name_inside_string_literal_does_not_keep_helper_alive() {
        // A string literal that happens to contain the helper name must not
        // be counted as a reference — the lowering relies on real
        // `IdentifierReference` AST nodes, never on textual matching.
        let source = "var _foo = _interopRequireDefault(require('./foo'));\n\
                      function _interopRequireDefault(e) { return e && e.__esModule ? e : { default: e }; }\n\
                      var description = '_interopRequireDefault is a babel helper';\n\
                      var entry = _foo.default;\n";
        let run = run_with_source(
            source,
            &["_foo", "_interopRequireDefault", "description", "entry"],
        );
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            !emitted.contains("function _interopRequireDefault"),
            "helper must be stripped despite the string literal mention; got:\n{emitted}",
        );
        assert!(
            emitted.contains("'_interopRequireDefault is a babel helper'"),
            "the unrelated string literal must be preserved verbatim; got:\n{emitted}",
        );
    }

    #[test]
    fn unknown_compiler_does_not_trigger_any_lowering() {
        // A module with no bundler signatures should be classified as
        // Unknown, which maps to `CompilerLowering::None`. No banner is
        // emitted and every top-level declaration must pass through
        // untouched. (Using bundler-named identifiers here would itself
        // trigger detection, so the fixture stays signature-free.)
        let source = "function compute(input) { return input * 2; }\n\
                      var entry = compute(21);\n\
                      var label = 'ready';\n";
        let run = run_with_source(source, &["compute", "entry", "label"]);
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            !emitted.contains("reverts-compiler-preserved"),
            "Unknown compiler must not emit any preservation banner; got:\n{emitted}",
        );
        assert!(
            emitted.contains("function compute"),
            "function declaration must be preserved; got:\n{emitted}",
        );
        assert!(
            emitted.contains("var entry = compute(21)"),
            "var declaration must be preserved; got:\n{emitted}",
        );
        assert!(
            emitted.contains("var label: string = 'ready'"),
            "string literal initializer must be preserved; got:\n{emitted}",
        );
    }

    #[test]
    fn esbuild_lowering_does_not_strip_babel_helper_definitions() {
        // Cross-compiler protection: when the detector picks esbuild, the
        // esbuild strip rules must only consider esbuild helper names. Any
        // babel-shaped helper that happens to live alongside esbuild
        // signatures must remain untouched.
        let source = "var __commonJS = (cb, mod) => function __require() { return mod; };\n\
                      function _interopRequireDefault(e) { return e && e.__esModule ? e : { default: e }; }\n\
                      var require_foo = __commonJS({ 'foo'() { return _interopRequireDefault({}); } });\n\
                      var entry = require_foo();\n";
        let run = run_with_source(
            source,
            &[
                "__commonJS",
                "_interopRequireDefault",
                "require_foo",
                "entry",
            ],
        );
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            emitted.contains("// reverts-compiler-preserved: esbuild"),
            "esbuild must be the active detection; got:\n{emitted}",
        );
        assert!(
            emitted.contains("function _interopRequireDefault"),
            "babel helper must survive when esbuild lowering is in charge; got:\n{emitted}",
        );
        assert!(
            emitted.contains("var __commonJS"),
            "referenced esbuild helper must survive; got:\n{emitted}",
        );
    }

    #[test]
    fn esbuild_helper_strip_does_not_run_from_iife_shape_alone() {
        // A top-level IIFE shape alone is not enough to infer esbuild. Without
        // module-scope runtime identifier evidence, helper stripping must not
        // run inside the wrapper.
        let source = "(() => {\n\
                        var __commonJS = (cb, mod) => function () { return mod; };\n\
                        var __defProp = Object.defineProperty;\n\
                        var __export = (target, all) => { return target; };\n\
                        var entry = 42;\n\
                      })();\n";
        let run = run_with_source(source, &[]);
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            !emitted.contains("// reverts-compiler-preserved: esbuild"),
            "IIFE shape alone must not carry esbuild banner; got:\n{emitted}",
        );
        for helper in ["__commonJS", "__defProp", "__export"] {
            assert!(
                emitted.contains(&format!("var {helper}")),
                "IIFE-internal helper `{helper}` must remain without esbuild evidence; got:\n{emitted}",
            );
        }
        assert!(
            emitted.contains("var entry: number = 42"),
            "unrelated IIFE-internal declarations must be preserved; got:\n{emitted}",
        );
    }

    #[test]
    fn webpack_helper_strip_does_not_run_from_iife_local_identifiers() {
        let source = "(() => {\n\
                        var __webpack_modules__ = {};\n\
                        var __webpack_module_cache__ = {};\n\
                        function __webpack_require__(id) { return id; }\n\
                        var entry = __webpack_require__(1);\n\
                      })();\n";
        let run = run_with_source(source, &[]);
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            !emitted.contains("// reverts-compiler-preserved: webpack"),
            "IIFE-local webpack identifiers must not trigger webpack banner; got:\n{emitted}",
        );
        assert!(
            emitted.contains("function __webpack_require__"),
            "referenced webpack helper must remain inside the IIFE; got:\n{emitted}",
        );
        for helper in ["__webpack_modules__", "__webpack_module_cache__"] {
            assert!(
                emitted.contains(&format!("var {helper}")),
                "unreferenced webpack helper `{helper}` must remain without webpack evidence; got:\n{emitted}",
            );
        }
        assert!(
            emitted.contains("var entry = __webpack_require__(1)"),
            "IIFE-internal call site must be preserved; got:\n{emitted}",
        );
    }

    #[test]
    fn webpack_lowering_does_not_run_from_iife_local_identifiers() {
        // `__webpack_require__.r(exports)` is webpack's CJS-to-ESM marker:
        // it sets `__esModule` on the exports object. In an ESM emit
        // context the call is either a no-op (when exports is bound) or a
        // runtime error (when it isn't). Lowering must drop it the same way
        // babel `__esModule` strip drops its sibling.
        let source = "(() => {\n\
                        function __webpack_require__(id) { return id; }\n\
                        __webpack_require__.r(__webpack_exports__);\n\
                        var entry = __webpack_require__(1);\n\
                      })();\n";
        let run = run_with_source(source, &[]);
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            !emitted.contains("// reverts-compiler-preserved: webpack"),
            "IIFE-local webpack identifiers must not trigger webpack banner; got:\n{emitted}",
        );
        assert!(
            emitted.contains("__webpack_require__.r("),
            "the marker call must remain without webpack evidence; got:\n{emitted}",
        );
        assert!(
            emitted.contains("var entry = __webpack_require__(1)"),
            "ordinary `__webpack_require__(N)` call sites must remain; got:\n{emitted}",
        );
        assert!(
            emitted.contains("function __webpack_require__"),
            "referenced helper must remain; got:\n{emitted}",
        );
    }

    #[test]
    fn terser_minified_source_emits_terser_banner() {
        // Long single-line source with low whitespace ratio triggers looks_minified
        // without matching any specific compiler runtime identifier.
        let source = "var a=function(b){return b?b.c?b.c.d:b.c:b};var c={};for(var d=0;d<200;d++)c[d]=a({c:{d:d}});var e=function(f){return f&&f.g?f.g.h:0};var x=c[0];var y=e(x);\n";
        let run = run_with_source(source, &["a", "c", "d", "e", "x", "y"]);
        assert!(run.audit.is_clean(), "audit: {:?}", run.audit.findings());
        let emitted = run.project.files[0].source.as_str();
        assert!(
            emitted.contains("// reverts-compiler-preserved: terser"),
            "terser fixture must carry terser banner, got:\n{emitted}",
        );
    }
}
