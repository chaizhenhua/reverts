//! `generate-project-v2` subcommand: load a project bundle from SQLite,
//! run the output pipeline, audit-gate the result, then materialise the
//! TypeScript project (sources, scaffold, assets) under `--output`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use clap::Args;
use reverts_model::IslandPackageExternalization;
use reverts_package_matcher::{IslandUnitAttribution, aggregate_island_packages};
use reverts_pipeline::{
    FunctionParamRenameRow, GenerateProjectOptions, LocalBindingRename,
    generate_project_from_prepared_with_options, prepare_and_enrich,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::args::{parse_args_with_name, parse_project_id};
use crate::commands::naming_gates::{NamingGateMode, validate_name_acceptance};
use crate::errors::{CliError, CliRunError};
use crate::format_audit_findings;
use crate::input_externalization::{
    load_materialized_package_manifests, load_project_bundle_with_package_externalization,
};
use crate::runtime_dependency_coherence::prune_transitively_provided_scope_incoherent_dependencies;
use crate::{collect_sqlite_rows, sqlite_table_exists, sqlite_table_has_column};

#[derive(Debug, Clone, PartialEq, Eq, Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct GenerateProjectV2Args {
    #[arg(long)]
    pub input: PathBuf,
    #[arg(long)]
    pub output: PathBuf,
    #[arg(long, value_parser = parse_project_id)]
    pub project_id: u32,
    /// Emit all generated source under this directory (e.g. `src`) for a modern
    /// TypeScript project layout: `moduleResolution: NodeNext`, a `package.json`
    /// `exports` map, `.gitignore`, and pipeline metadata
    /// (`symbol-index.json`, `binding-name-index.json`) relocated to `.reverts/`
    /// so the source tree stays clean. Omit it for the flat legacy layout.
    #[arg(long)]
    pub source_root: Option<String>,
}

impl GenerateProjectV2Args {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let mut args = args.into_iter().collect::<Vec<_>>();
        if args
            .first()
            .is_some_and(|argument| argument == crate::help::GENERATE_PROJECT_V2_COMMAND)
        {
            args.remove(0);
        }
        parse_args_with_name(crate::help::GENERATE_PROJECT_V2_COMMAND, args)
    }
}

pub(crate) fn run(args: GenerateProjectV2Args) -> Result<(), CliRunError> {
    let timing_enabled = std::env::var_os("REVERTS_GENERATE_TIMING").is_some();
    let started = std::time::Instant::now();
    let mut last = started;
    macro_rules! mark_timing {
        ($stage:literal) => {
            if timing_enabled {
                let now = std::time::Instant::now();
                eprintln!(
                    "generate-project cli timing: {} stage={:.3}s total={:.3}s",
                    $stage,
                    now.duration_since(last).as_secs_f64(),
                    now.duration_since(started).as_secs_f64()
                );
                last = now;
            }
        };
    }
    validate_accepted_naming_gate_records(&args.input, args.project_id)?;
    mark_timing!("validate_naming");
    let input = load_project_bundle_with_package_externalization(&args.input, args.project_id)
        .map_err(CliRunError::LoadInput)?;
    mark_timing!("load_input");
    let local_binding_renames = load_local_binding_renames(&args.input, args.project_id)?;
    mark_timing!("load_binding_renames");
    let function_param_renames = load_function_param_renames(&args.input, args.project_id)?;
    mark_timing!("load_param_renames");
    let package_anchored_island_bindings =
        load_package_anchored_island_bindings(&args.input, args.project_id)?;
    let island_unit_attributions = load_island_unit_attributions(&args.input, args.project_id)?;
    let island_index_maps = load_package_index_reexports(&args.input, &island_unit_attributions)?;
    mark_timing!("load_island_anchors");

    // Recover inlined packages from the island and attach them to the program so
    // the planner replaces them with bare imports. Aggregation needs the runtime
    // prelude, available only after enrichment, so build the prepared program
    // first, then aggregate, then generate.
    let mut prepared = prepare_and_enrich(input).map_err(CliRunError::Pipeline)?;
    let externalizations = island_package_externalizations(
        &prepared.program,
        &island_unit_attributions,
        &island_index_maps,
    );
    if !externalizations.is_empty() {
        prepared.program = prepared
            .program
            .with_island_package_externalizations(externalizations);
    }
    mark_timing!("aggregate_island_packages");
    let run = generate_project_from_prepared_with_options(
        prepared,
        GenerateProjectOptions {
            local_binding_renames: local_binding_renames.clone(),
            function_param_renames,
            package_anchored_island_bindings,
        },
    )
    .map_err(CliRunError::Pipeline)?;
    mark_timing!("pipeline");

    // Only errors block writing the output. Warnings (e.g. duplicate
    // top-level binding, ambiguous binding shape) describe input-bundle
    // conditions per ADR 0002: surface them, don't strand the user.
    if run.audit.has_errors() {
        return Err(CliRunError::AuditRejected(format_audit_findings(
            &run.audit,
        )));
    }
    if !run.audit.is_clean() {
        eprintln!(
            "warning: generated project carries {} audit warning(s):\n{}",
            run.audit.warning_count(),
            format_audit_findings(&run.audit)
        );
    }

    let accepted_project = run
        .accepted_project
        .ok_or_else(|| CliRunError::AuditRejected(format_audit_findings(&run.audit)))?;
    // Drop scope-incoherent root pins (e.g. a mis-matched off-major `@smithy/*`
    // sibling) that npm would otherwise install transitively at a coherent
    // version; root-pinning conflicting majors of one scope blows up `npm
    // install`. Needs the cached `package.json` dependency graph, which only
    // the CLI (not the generation pipeline) can see.
    let manifests =
        load_materialized_package_manifests(&args.input).map_err(CliRunError::LoadInput)?;
    mark_timing!("load_manifests");
    let runtime_dependencies = prune_transitively_provided_scope_incoherent_dependencies(
        run.runtime_dependencies.clone(),
        &manifests,
    );
    mark_timing!("prune_dependencies");
    let mut assets = run.assets.clone();
    assets.extend(run.source_mirror_assets.clone());
    let source_root = args.source_root.as_deref();
    let written = write_accepted_project(
        &accepted_project,
        assets.as_slice(),
        &args.output,
        &runtime_dependencies,
        source_root,
    )?;
    mark_timing!("write_project");
    // Pipeline metadata is not part of the published source tree: in the modern
    // (source-root) layout it lives in a `.reverts/` sidecar; the flat layout
    // keeps it at the output root for backward compatibility.
    let metadata_dir = match source_root {
        Some(_) => args.output.join(".reverts"),
        None => args.output.clone(),
    };
    std::fs::create_dir_all(&metadata_dir).map_err(|source| CliRunError::WriteOutput {
        path: metadata_dir.clone(),
        source,
    })?;
    let symbol_index_path = metadata_dir.join("symbol-index.json");
    std::fs::write(
        &symbol_index_path,
        serialize_symbol_index(&run.symbol_index),
    )
    .map_err(|source| CliRunError::WriteOutput {
        path: symbol_index_path.clone(),
        source,
    })?;
    let binding_name_index_path = metadata_dir.join("binding-name-index.json");
    std::fs::write(
        &binding_name_index_path,
        serialize_binding_name_index(&local_binding_renames),
    )
    .map_err(|source| CliRunError::WriteOutput {
        path: binding_name_index_path.clone(),
        source,
    })?;
    // The `cluster-names` worklist: fingerprint → emitted island path, so a
    // naming agent can read each `island/…` file and accept a semantic path keyed
    // by the stable fingerprint (which survives the rename).
    let island_clusters_path = metadata_dir.join("island-clusters.json");
    std::fs::write(
        &island_clusters_path,
        serialize_island_cluster_manifest(&run.island_clusters),
    )
    .map_err(|source| CliRunError::WriteOutput {
        path: island_clusters_path.clone(),
        source,
    })?;
    // Source-restoration M1: identify every module recognized as third-party
    // (package ownership matched, even when NOT externalizable) and drop the real
    // npm source beside the output for reference. Pure sidecar — never alters the
    // running code, so it is always safe. See docs/source-restoration-plan.md.
    let restored =
        write_recognized_package_sources(&args.input, &run.module_output_paths, &metadata_dir)?;
    println!(
        "generated project {} into {} with {written} files ({} symbol-index entries; {restored} recognized-package source(s) restored)",
        args.project_id,
        args.output.display(),
        run.symbol_index.len()
    );
    if timing_enabled {
        let _ = last;
    }
    Ok(())
}

fn load_local_binding_renames(
    input: &Path,
    project_id: u32,
) -> Result<Vec<LocalBindingRename>, CliRunError> {
    let connection = Connection::open_with_flags(input, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    if !sqlite_table_exists(&connection, "semantic_binding_names")
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?
    {
        return Ok(Vec::new());
    }
    let has_binding_key =
        sqlite_column_exists(&connection, "semantic_binding_names", "binding_key")?;
    // A DB without the gate_status column predates the quality gate; grandfather its
    // accepted names (load them ungated) instead of refusing to generate. Gate-aware
    // DBs keep the strict `gate_status = 'passed'` filter.
    let gate_clause = if sqlite_column_exists(&connection, "semantic_binding_names", "gate_status")?
    {
        "AND gate_status = 'passed'"
    } else {
        ""
    };
    let query = if has_binding_key {
        format!(
            "SELECT file_path, original_name, binding_index, semantic_name
            FROM semantic_binding_names
            WHERE project_id = ?1
              AND accepted = 1
              {gate_clause}
              AND TRIM(file_path) != ''
              AND TRIM(original_name) != ''
              AND TRIM(semantic_name) != ''
            ORDER BY file_path, original_name, binding_key"
        )
    } else {
        format!(
            "SELECT file_path, original_name, NULL AS binding_index, semantic_name
            FROM semantic_binding_names
            WHERE project_id = ?1
              AND accepted = 1
              {gate_clause}
              AND TRIM(file_path) != ''
              AND TRIM(original_name) != ''
              AND TRIM(semantic_name) != ''
            ORDER BY file_path, original_name"
        )
    };
    let mut statement = connection
        .prepare(query.as_str())
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let rows = statement
        .query_map(params![i64::from(project_id)], |row| {
            Ok(LocalBindingRename {
                file_path: row.get(0)?,
                original_name: row.get(1)?,
                binding_index: row
                    .get::<_, Option<i64>>(2)?
                    .and_then(|value| u32::try_from(value).ok()),
                semantic_name: row.get(3)?,
            })
        })
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    collect_sqlite_rows(rows).map_err(|source| CliRunError::GenerateProject(source.to_string()))
}

/// Bundle-local names of eager entry-island bindings the matcher anchored to a
/// third-party library (`package_island_anchors`). These are library code, not
/// application symbols, so generation drops them from the naming denominator.
/// A pre-anchoring database (no table) simply contributes nothing.
fn load_package_anchored_island_bindings(
    input: &Path,
    project_id: u32,
) -> Result<std::collections::BTreeSet<String>, CliRunError> {
    let connection = Connection::open_with_flags(input, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    if !sqlite_table_exists(&connection, "package_island_anchors")
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?
    {
        return Ok(std::collections::BTreeSet::new());
    }
    let mut statement = connection
        .prepare(
            "SELECT DISTINCT binding_name FROM package_island_anchors \
             WHERE project_id = ?1 AND TRIM(binding_name) != ''",
        )
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let rows = statement
        .query_map(params![i64::from(project_id)], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let mut names = std::collections::BTreeSet::new();
    for row in rows {
        names.insert(row.map_err(|source| CliRunError::GenerateProject(source.to_string()))?);
    }
    Ok(names)
}

/// Per-binding island package attributions (`package_island_anchors`), the
/// input the island aggregation groups into per-package externalization plans.
fn load_island_unit_attributions(
    input: &Path,
    project_id: u32,
) -> Result<Vec<IslandUnitAttribution>, CliRunError> {
    let connection = Connection::open_with_flags(input, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    if !sqlite_table_exists(&connection, "package_island_anchors")
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?
    {
        return Ok(Vec::new());
    }
    let mut statement = connection
        .prepare(
            "SELECT DISTINCT binding_name, package_name, package_version, export_specifier \
             FROM package_island_anchors \
             WHERE project_id = ?1 AND TRIM(binding_name) != '' AND TRIM(package_name) != ''",
        )
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let rows = statement
        .query_map(params![i64::from(project_id)], |row| {
            Ok(IslandUnitAttribution {
                binding: row.get::<_, String>(0)?,
                package_name: row.get::<_, String>(1)?,
                package_version: row.get::<_, String>(2)?,
                export_specifier: row.get::<_, String>(3).unwrap_or_default(),
            })
        })
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let mut attributions = Vec::new();
    for row in rows {
        attributions.push(row.map_err(|source| CliRunError::GenerateProject(source.to_string()))?);
    }
    Ok(attributions)
}

/// Load and parse the real `index.js` re-export map for each attributed package,
/// from the persisted `package_source_cache`. Used to synthesize a barrel for a
/// tree-shaken package whose in-bundle barrel was dropped. Packages with no
/// cached index (or an unparseable one) are simply absent — those fall back to
/// the recovered-barrel path (and skip if none).
fn load_package_index_reexports(
    input: &Path,
    attributions: &[IslandUnitAttribution],
) -> Result<BTreeMap<(String, String), reverts_js::PackageIndexReexports>, CliRunError> {
    if attributions.is_empty() {
        return Ok(BTreeMap::new());
    }
    let connection = Connection::open_with_flags(input, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    if !sqlite_table_exists(&connection, "package_source_cache")
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?
    {
        return Ok(BTreeMap::new());
    }
    let packages: BTreeSet<(String, String)> = attributions
        .iter()
        .map(|attribution| {
            (
                attribution.package_name.clone(),
                attribution.package_version.clone(),
            )
        })
        .collect();
    let debug = std::env::var_os("REVERTS_DEBUG_ISLAND_PKG").is_some();
    let mut maps = BTreeMap::new();
    for (package, version) in packages {
        // The package-root barrel first (existing behavior for all packages).
        let root = load_cache_entry_first(
            &connection,
            &package,
            &version,
            &["index.js", "dist/index.js", "lib/index.js", "src/index.js"],
        )?
        .map(|source| reverts_js::parse_index_reexports(&source))
        .unwrap_or_default();
        if !root.is_empty() {
            maps.insert((package, version), root);
            continue;
        }
        // Guard/empty root (e.g. @sentry/electron index.js only throws "use /main
        // or /renderer") -> recover the package's public SUBPATH barrels instead.
        let mut merged = reverts_js::PackageIndexReexports::default();
        for subpath in subpath_entries_from_anchors(&package, attributions) {
            let candidates = [format!("{subpath}/index.js"), format!("{subpath}/index.cjs")];
            let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();
            let Some(source) = load_cache_entry_first(&connection, &package, &version, &refs)? else {
                continue;
            };
            let parsed = reverts_js::parse_index_reexports(&source);
            if parsed.is_empty() {
                continue;
            }
            let specifier = format!("{package}/{subpath}");
            merged.merge(parsed.rebased(&subpath, &specifier));
            if debug {
                eprintln!("island-pkg subpath-barrel {package}: loaded {subpath}/index.js");
            }
        }
        if !merged.is_empty() {
            maps.insert((package, version), merged);
        }
    }
    Ok(maps)
}

/// First cached source among `entry_paths` (in the order given), or `None`.
fn load_cache_entry_first(
    connection: &Connection,
    package: &str,
    version: &str,
    entry_paths: &[&str],
) -> Result<Option<String>, CliRunError> {
    for entry in entry_paths {
        let source: Option<String> = connection
            .query_row(
                "SELECT source_content FROM package_source_cache \
                 WHERE package_name = ?1 AND package_version = ?2 AND entry_path = ?3 LIMIT 1",
                params![package, version, entry],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
        if source.is_some() {
            return Ok(source);
        }
    }
    Ok(None)
}

/// The package's candidate public-subpath directories, derived from its anchored
/// submodules: the first path segment of each anchor's `export_specifier` (after
/// the `<pkg>/` prefix), keeping only single-segment NESTED entries that are real
/// public subpaths -- not bundler build-output dirs (`esm`, `cjs`, `dist`, ...),
/// which are internal and not importable.
fn subpath_entries_from_anchors(
    package: &str,
    attributions: &[IslandUnitAttribution],
) -> BTreeSet<String> {
    const BUILD_DIRS: &[&str] = &[
        "esm", "cjs", "dist", "lib", "build", "src", "types", "umd", "browser", "node", "module",
        "internal",
    ];
    let mut dirs = BTreeSet::new();
    for attribution in attributions {
        if attribution.package_name != package || attribution.export_specifier.is_empty() {
            continue;
        }
        let relpath = attribution
            .export_specifier
            .strip_prefix(package)
            .and_then(|rest| rest.strip_prefix('/'))
            .unwrap_or(attribution.export_specifier.as_str());
        let mut segments = relpath.split('/');
        let (Some(first), Some(_)) = (segments.next(), segments.next()) else {
            continue; // a top-level file, not a nested subpath submodule
        };
        if !first.is_empty() && !BUILD_DIRS.contains(&first) {
            dirs.insert(first.to_string());
        }
    }
    dirs
}

/// Aggregate per-binding island attributions into per-package externalization
/// plans over every runtime prelude, keeping only the safely externalizable
/// ones (a barrel recovered or synthesized). Converted to the planner-facing model type.
fn island_package_externalizations(
    program: &reverts_model::EnrichedProgram,
    attributions: &[IslandUnitAttribution],
    index_maps: &BTreeMap<(String, String), reverts_js::PackageIndexReexports>,
) -> Vec<IslandPackageExternalization> {
    if attributions.is_empty() {
        return Vec::new();
    }
    let mut externalizations = Vec::new();
    for prelude in program.model().graph().runtime_preludes().values() {
        for plan in aggregate_island_packages(prelude, attributions, index_maps) {
            if !plan.externalizable {
                eprintln!(
                    "island-package skip: {} ({} member binding(s)){}",
                    plan.import_specifier,
                    plan.member_bindings.len(),
                    plan.skip_reason
                        .as_deref()
                        .map(|reason| format!(": {reason}"))
                        .unwrap_or_default()
                );
                continue;
            }
            eprintln!(
                "island-package externalize: {} ({} member binding(s)){}",
                plan.import_specifier,
                plan.member_bindings.len(),
                if plan.synthesized_members.is_empty() {
                    ""
                } else {
                    " (synthesized barrel)"
                }
            );
            let synthesized_members = plan
                .synthesized_members
                .into_iter()
                .map(|member| reverts_model::SynthesizedMemberExternalization {
                    init_fn: reverts_ir::BindingName::new(member.init_fn),
                    local_binding: reverts_ir::BindingName::new(member.local_binding),
                    namespace_members: member.namespace_members,
                    import_specifier: member.import_specifier,
                })
                .collect();
            externalizations.push(IslandPackageExternalization {
                import_specifier: plan.import_specifier,
                version: plan.version,
                entry_init: reverts_ir::BindingName::new(plan.entry_init),
                entry_exports: reverts_ir::BindingName::new(plan.entry_exports),
                member_bindings: plan
                    .member_bindings
                    .into_iter()
                    .map(reverts_ir::BindingName::new)
                    .collect(),
                synthesized_members,
            });
        }
    }
    externalizations
}

fn load_function_param_renames(
    input: &Path,
    project_id: u32,
) -> Result<Vec<FunctionParamRenameRow>, CliRunError> {
    let connection = Connection::open_with_flags(input, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    if !sqlite_table_exists(&connection, "semantic_function_param_names")
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?
    {
        return Ok(Vec::new());
    }
    let mut statement = connection
        .prepare(
            r"
            SELECT file_path, function_name, param_index, semantic_name
            FROM semantic_function_param_names
            WHERE project_id = ?1
              AND accepted = 1
              AND NULLIF(TRIM(semantic_name), '') IS NOT NULL
            ",
        )
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let rows = statement
        .query_map(params![i64::from(project_id)], |row| {
            Ok(FunctionParamRenameRow {
                file_path: row.get(0)?,
                function_name: row.get(1)?,
                param_index: u32::try_from(row.get::<_, i64>(2)?).unwrap_or(u32::MAX),
                semantic_name: row.get(3)?,
            })
        })
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    collect_sqlite_rows(rows).map_err(|source| CliRunError::GenerateProject(source.to_string()))
}

fn validate_accepted_naming_gate_records(input: &Path, project_id: u32) -> Result<(), CliRunError> {
    let connection = Connection::open_with_flags(input, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    validate_active_symbol_names_have_passed_gates(&connection, project_id)?;
    validate_accepted_binding_names_have_passed_gates(&connection, project_id)
}

fn validate_active_symbol_names_have_passed_gates(
    connection: &Connection,
    project_id: u32,
) -> Result<(), CliRunError> {
    if !sqlite_table_exists(connection, "symbols")
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?
    {
        return Ok(());
    }
    let has_source_column = sqlite_table_has_column(connection, "symbols", "semantic_name_source")
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let source_expr = if has_source_column {
        "NULLIF(TRIM(s.semantic_name_source), '') AS semantic_name_source"
    } else {
        "NULL AS semantic_name_source"
    };
    let sql = format!(
        r"
        SELECT s.module_id, s.original_name, NULLIF(TRIM(s.semantic_name), '') AS semantic_name,
               {source_expr}
        FROM symbols s
        JOIN modules m ON m.id = s.module_id
        JOIN project_files pf ON pf.file_id = m.file_id
        WHERE pf.project_id = ?1
          AND s.scope_level = 'module'
          AND NULLIF(TRIM(s.semantic_name), '') IS NOT NULL
        ORDER BY s.module_id, s.original_name
        "
    );
    let mut statement = connection
        .prepare(sql.as_str())
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let rows = statement
        .query_map(params![i64::from(project_id)], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let rows = collect_sqlite_rows(rows)
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    // A name with no `semantic_name_source` is a pre-gate / legacy name (applied
    // before the proposal-provenance system existed, e.g. a project imported and
    // agent-named under an older schema). It makes no provenance claim and no
    // matching proposal can exist (proposals join on `origin`), so HARD-blocking
    // generation on it would make every legacy project ungeneratable — a regression,
    // not a safety win. Grandfather those by enforcing only names that DO claim a
    // source: properly gate-applied names stay fully validated, and a name that
    // fakes a source without a matching proposal is still caught below.
    let provenanced: Vec<_> = rows
        .into_iter()
        .filter(|(_, _, _, origin)| origin.is_some())
        .collect();
    if provenanced.is_empty() {
        return Ok(());
    }
    if !sqlite_table_exists(connection, "symbol_name_proposals")
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?
        || !sqlite_table_has_column(connection, "symbol_name_proposals", "gate_status")
            .map_err(|source| CliRunError::GenerateProject(source.to_string()))?
    {
        // The proposal table predates the gate-status mechanism: this project was
        // named under an older schema where the quality gate did not exist. Its
        // active names cannot be validated against a gate that was never present
        // and were applied before it — grandfather the whole pre-gate project (same
        // rationale as the source-less legacy names above) rather than making every
        // older decompile DB ungeneratable. Gate-aware DBs (column present) are
        // still fully validated below.
        return Ok(());
    }
    for (module_id, original_name, semantic_name, origin) in provenanced {
        let origin = origin.expect("filtered to provenanced (origin = Some) rows above");
        let proposal_evidence = connection
            .query_row(
                r"
                SELECT evidence
                FROM symbol_name_proposals
                WHERE project_id = ?1
                  AND module_id = ?2
                  AND original_name = ?3
                  AND semantic_name = ?4
                  AND origin = ?5
                  AND accepted = 1
                  AND gate_status = 'passed'
                LIMIT 1
                ",
                params![
                    i64::from(project_id),
                    module_id,
                    original_name.as_str(),
                    semantic_name.as_str(),
                    origin.as_str(),
                ],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
        let Some(evidence) = proposal_evidence else {
            return Err(CliRunError::GenerateProject(format!(
                "active semantic name {module_id}:{original_name} -> {semantic_name} has no matching accepted gate-passed proposal"
            )));
        };
        validate_name_acceptance(
            original_name.as_str(),
            semantic_name.as_str(),
            origin.as_str(),
            evidence.as_deref(),
            NamingGateMode::Symbol,
        )
        .map_err(|error| CliRunError::GenerateProject(error.message()))?;
    }
    Ok(())
}

fn validate_accepted_binding_names_have_passed_gates(
    connection: &Connection,
    project_id: u32,
) -> Result<(), CliRunError> {
    if !sqlite_table_exists(connection, "semantic_binding_names")
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?
    {
        return Ok(());
    }
    if !sqlite_table_has_column(connection, "semantic_binding_names", "gate_status")
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?
    {
        // Pre-gate DB (no gate_status column): its accepted names were applied before
        // the quality gate existed — grandfather them rather than blocking generation.
        return Ok(());
    }
    let has_binding_key =
        sqlite_table_has_column(connection, "semantic_binding_names", "binding_key")
            .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let mut statement = connection
        .prepare(if has_binding_key {
            r"
            SELECT file_path, original_name, binding_index, semantic_name, origin, evidence, gate_status
            FROM semantic_binding_names
            WHERE project_id = ?1 AND accepted = 1
            ORDER BY file_path, original_name, binding_key
            "
        } else {
            r"
            SELECT file_path, original_name, NULL AS binding_index, semantic_name, origin, evidence, gate_status
            FROM semantic_binding_names
            WHERE project_id = ?1 AND accepted = 1
            ORDER BY file_path, original_name
            "
        })
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let rows = statement
        .query_map(params![i64::from(project_id)], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
            ))
        })
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    for row in collect_sqlite_rows(rows)
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?
    {
        let (file_path, original_name, _binding_index, semantic_name, origin, evidence, status) =
            row;
        if status != "passed" {
            return Err(CliRunError::GenerateProject(format!(
                "accepted binding name {file_path}:{original_name} -> {semantic_name} has gate_status={status}, expected passed"
            )));
        }
        validate_name_acceptance(
            original_name.as_str(),
            semantic_name.as_str(),
            origin.as_str(),
            evidence.as_deref(),
            NamingGateMode::LocalBinding,
        )
        .map_err(|error| CliRunError::GenerateProject(error.message()))?;
    }
    Ok(())
}

fn sqlite_column_exists(
    connection: &Connection,
    table: &str,
    column: &str,
) -> Result<bool, CliRunError> {
    let mut statement = connection
        .prepare(format!("PRAGMA table_info({table})").as_str())
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let columns = collect_sqlite_rows(rows)
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    Ok(columns.iter().any(|existing| existing == column))
}

fn serialize_binding_name_index(entries: &[LocalBindingRename]) -> String {
    let rows = entries
        .iter()
        .map(|entry| {
            serde_json::json!({
                "file_path": entry.file_path,
                "original_name": entry.original_name,
                "binding_index": entry.binding_index,
                "emitted_name": entry.semantic_name,
                "semantic_named": true,
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string_pretty(&serde_json::Value::Array(rows))
        .expect("serializing a JSON array of plain values is infallible")
}

/// Serializes the symbol index as a JSON array. `reverts_pipeline::SymbolIndexEntry`
/// has no serde derive (the pipeline crate stays serde-free), so the CLI renders
/// the rows it needs.
fn serialize_symbol_index(entries: &[reverts_pipeline::SymbolIndexEntry]) -> String {
    let rows: Vec<serde_json::Value> = entries
        .iter()
        .map(|entry| {
            serde_json::json!({
                // `null` for bindings in unmodularized recovered-code files
                // (e.g. the eager entrypoint island) — named via the
                // file-path-keyed binding-name channel, not the symbols table.
                "module_id": entry.module_id.map(|module_id| module_id.0),
                "original_name": entry.original_name,
                "emitted_name": entry.emitted_name,
                "semantic_named": entry.semantic_named,
                "file_path": entry.file_path,
                "function_like": entry.function_like,
                "dead": entry.dead,
                "exported": entry.exported,
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::Value::Array(rows))
        .expect("serializing a JSON array of plain values is infallible")
}

/// Serializes the island-cluster manifest as a JSON array of
/// `{fingerprint, path, binding_count}`. The `cluster-names` agent reads each
/// `path`, inspects the file, and accepts a semantic path keyed by `fingerprint`.
fn serialize_island_cluster_manifest(
    clusters: &[reverts_pipeline::IslandClusterRecord],
) -> String {
    let mut sorted: Vec<&reverts_pipeline::IslandClusterRecord> = clusters.iter().collect();
    sorted.sort_by(|a, b| b.binding_count.cmp(&a.binding_count).then(a.path.cmp(&b.path)));
    let rows: Vec<serde_json::Value> = sorted
        .iter()
        .map(|record| {
            serde_json::json!({
                "fingerprint": record.fingerprint,
                "path": record.path,
                "binding_count": record.binding_count,
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::Value::Array(rows))
        .expect("serializing a JSON array of plain values is infallible")
}

/// Source-restoration M1. For every module recognized as third-party (a
/// `package_attributions` row that matched package ownership — accepted OR
/// rejected-but-matched), write a `.reverts/recognized-packages.json` manifest
/// (module → package@version/subpath, externalized?) and drop the real npm
/// source from `package_source_cache` under `.reverts/restored-sources/`. This
/// never touches the emitted, running code — it only ANNOTATES which inlined
/// modules are which library and provides the readable original for reference.
/// Returns the number of restored sources written.
fn write_recognized_package_sources(
    input: &Path,
    module_output_paths: &BTreeMap<reverts_ir::ModuleId, String>,
    metadata_dir: &Path,
) -> Result<usize, CliRunError> {
    let connection = Connection::open_with_flags(input, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    if !sqlite_table_exists(&connection, "package_attributions")
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?
    {
        std::fs::write(metadata_dir.join("recognized-packages.json"), "[]\n").ok();
        return Ok(0);
    }

    // Recognized modules: a real package + version, any status (accepted means
    // externalized; rejected with a matched package means recognized-but-inlined).
    let mut statement = connection
        .prepare(
            r"
            SELECT module_id, package_name, package_version, package_subpath, status
            FROM package_attributions
            WHERE package_name IS NOT NULL AND TRIM(package_name) != ''
              AND package_version IS NOT NULL AND TRIM(package_version) != ''
            ",
        )
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;

    // De-dup by (module, package, subpath); prefer an accepted status if present.
    let mut recognized: BTreeMap<(i64, String, String), (String, String, bool)> = BTreeMap::new();
    for row in rows {
        let (module_id, package, version, subpath, status) =
            row.map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
        let subpath = subpath.unwrap_or_default();
        let accepted = status == "accepted";
        recognized
            .entry((module_id, package.clone(), subpath.clone()))
            .and_modify(|entry| entry.2 |= accepted)
            .or_insert((version.clone(), status.clone(), accepted));
    }
    if recognized.is_empty() {
        std::fs::write(metadata_dir.join("recognized-packages.json"), "[]\n").ok();
        return Ok(0);
    }

    let restored_dir = metadata_dir.join("restored-sources");
    let mut manifest = Vec::<serde_json::Value>::new();
    let mut written_sources = 0usize;
    let mut wrote_paths = std::collections::BTreeSet::<String>::new();
    for ((module_id, package, subpath), (version, status, accepted)) in &recognized {
        let module_path = module_output_paths
            .get(&reverts_ir::ModuleId(u32::try_from(*module_id).unwrap_or(0)))
            .cloned();
        // The real source for this submodule, from the cache.
        let restored_rel = restored_source_for(&connection, package, version, subpath)
            .ok()
            .flatten()
            .map(|source| {
                let safe = sanitize_restored_path(package, version, subpath);
                let dest = restored_dir.join(&safe);
                if wrote_paths.insert(safe.clone()) {
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent).ok();
                    }
                    if std::fs::write(&dest, source).is_ok() {
                        written_sources += 1;
                    }
                }
                format!("restored-sources/{safe}")
            });
        manifest.push(serde_json::json!({
            "module_id": module_id,
            "file_path": module_path,
            "package": package,
            "version": version,
            "subpath": subpath,
            "externalized": accepted,
            "status": if *accepted { "externalized" } else { status.as_str() },
            "restored_source": restored_rel,
            // M2: the package-aware path this recognized module *would* occupy in a
            // vendored layout (`vendor/<pkg>/<subpath>.ts`). Deterministic and
            // gate-safe; an annotation only — the emitted file is unchanged.
            "vendor_path": vendor_module_path(package, subpath),
        }));
    }
    let manifest_json = serde_json::to_string_pretty(&serde_json::Value::Array(manifest))
        .expect("serializing recognized-packages manifest is infallible");
    std::fs::write(metadata_dir.join("recognized-packages.json"), manifest_json).map_err(
        |source| CliRunError::WriteOutput {
            path: metadata_dir.join("recognized-packages.json"),
            source,
        },
    )?;

    // Most recognized modules are island-inlined with NO per-module subpath (e.g.
    // 469 @opentelemetry/api attributions all NULL subpath), so a per-module
    // restore reaches almost none. Instead dump each recognized package's COMPLETE
    // real source tree from the cache — so every identified package's full readable
    // npm source sits under `.reverts/restored-sources/<pkg>@<ver>/…`, regardless
    // of whether its code was inlined into the scope-hoisted island.
    let packages: std::collections::BTreeSet<(String, String)> = recognized
        .iter()
        .map(|((_, package, _), (version, _, _))| (package.clone(), version.clone()))
        .collect();
    let mut tree_statement = connection
        .prepare(
            "SELECT entry_path, source_content FROM package_source_cache \
             WHERE package_name = ?1 AND package_version = ?2",
        )
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    for (package, version) in packages {
        let files = tree_statement
            .query_map(params![package, version], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
        for file in files {
            let (entry_path, source) =
                file.map_err(|err| CliRunError::GenerateProject(err.to_string()))?;
            let safe = sanitize_restored_path(&package, &version, &entry_path);
            if !wrote_paths.insert(safe.clone()) {
                continue;
            }
            let dest = restored_dir.join(&safe);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            if std::fs::write(&dest, source).is_ok() {
                written_sources += 1;
            }
        }
    }
    Ok(written_sources)
}

/// The cached real source for a package submodule, by best-matching entry path.
fn restored_source_for(
    connection: &Connection,
    package: &str,
    version: &str,
    subpath: &str,
) -> Result<Option<String>, CliRunError> {
    let entry = if subpath.is_empty() {
        "index.js".to_string()
    } else {
        subpath.to_string()
    };
    // The cache may key a submodule under a build-output prefix (build/esm/…,
    // lib/…, dist/…) rather than the bare subpath, so fall back to a basename
    // match before giving up. Exact key first (tightest).
    let basename = entry.rsplit('/').next().unwrap_or(entry.as_str());
    let source: Option<String> = connection
        .query_row(
            r"
            SELECT source_content FROM package_source_cache
            WHERE package_name = ?1 AND package_version = ?2
              AND (entry_path = ?3 OR export_specifier = ?4 OR entry_path LIKE ?5)
            ORDER BY (entry_path = ?3) DESC, length(entry_path) LIMIT 1
            ",
            params![
                package,
                version,
                entry,
                format!("{package}/{subpath}"),
                format!("%/{basename}")
            ],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    Ok(source)
}

/// A safe relative path under `restored-sources/` for a package submodule.
fn sanitize_restored_path(package: &str, version: &str, subpath: &str) -> String {
    let clean = |segment: &str| -> String {
        segment
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric()
                    || ch == '.'
                    || ch == '_'
                    || ch == '-'
                    || ch == '/'
                    || ch == '@'
                {
                    ch
                } else {
                    '-'
                }
            })
            .collect::<String>()
    };
    let sub = if subpath.is_empty() {
        "index.js".to_string()
    } else {
        clean(subpath)
    };
    let sub = sub.trim_start_matches('/');
    format!("{}@{}/{}", clean(package), clean(version), sub)
}

/// Source-restoration M2. The package-aware path a recognized third-party module
/// would occupy in a vendored layout: `vendor/<package>/<subpath>.ts`. The result
/// is guaranteed safe for both the module-path naming gate
/// (`validate_module_path_acceptance`) and the output-path layout
/// (`is_safe_typescript_module_path`): every segment is non-empty, never `.`/`..`,
/// the scope `@` is dropped, and the extension is normalized to `.ts`. A NULL
/// subpath (the common island-inlined case) falls back to `index`.
fn vendor_module_path(package: &str, subpath: &str) -> String {
    // A path segment reduced to the gate-safe alphabet (alphanumeric + `_-.`),
    // with runs of replaced characters collapsed and `.`/`-` trimmed at the ends
    // so no segment is empty, `.`, or `..`.
    let segment = |raw: &str| -> Option<String> {
        let mut out = String::with_capacity(raw.len());
        let mut last_dash = false;
        for ch in raw.chars() {
            let mapped = if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.') {
                ch
            } else {
                '-'
            };
            if mapped == '-' {
                if last_dash {
                    continue;
                }
                last_dash = true;
            } else {
                last_dash = false;
            }
            out.push(mapped);
        }
        let trimmed = out.trim_matches(|ch| matches!(ch, '-' | '.')).to_string();
        (!trimmed.is_empty()).then_some(trimmed)
    };
    let segments = |raw: &str| -> Vec<String> { raw.split('/').filter_map(&segment).collect() };

    let mut parts = vec!["vendor".to_string()];
    parts.extend(segments(package));
    // Drop any source extension from the subpath; we re-append `.ts` below.
    let stem = subpath
        .trim()
        .trim_end_matches('/')
        .rsplit_once('.')
        .map(|(head, ext)| {
            if matches!(ext, "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts") {
                head
            } else {
                subpath.trim()
            }
        })
        .unwrap_or_else(|| subpath.trim());
    let mut tail = segments(stem);
    if tail.is_empty() {
        tail.push("index".to_string());
    }
    parts.extend(tail);
    format!("{}.ts", parts.join("/"))
}

pub(crate) use crate::project_writer::write_accepted_project;

#[cfg(test)]
pub(crate) use crate::project_writer::{
    checked_output_path, write_emitted_project, write_emitted_project_with_source_root,
};

#[cfg(test)]
mod tests {
    use super::{
        restored_source_for, sanitize_restored_path, validate_accepted_naming_gate_records,
        vendor_module_path, write_recognized_package_sources,
    };
    use crate::commands::naming_gates::validate_module_path_acceptance;

    use std::collections::BTreeMap;

    use rusqlite::{Connection, OpenFlags, params};
    use tempfile::TempDir;

    fn gate_db() -> (TempDir, std::path::PathBuf) {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("input.sqlite");
        let connection = Connection::open(path.as_path()).expect("open sqlite");
        connection
            .execute_batch(
                r"
                CREATE TABLE project_files (
                    project_id INTEGER NOT NULL,
                    file_id INTEGER NOT NULL
                );
                CREATE TABLE modules (
                    id INTEGER PRIMARY KEY,
                    file_id INTEGER NOT NULL
                );
                CREATE TABLE symbols (
                    module_id INTEGER NOT NULL,
                    original_name TEXT NOT NULL,
                    semantic_name TEXT,
                    semantic_name_source TEXT,
                    export_name TEXT,
                    scope_level TEXT
                );
                CREATE TABLE symbol_name_proposals (
                    project_id INTEGER NOT NULL,
                    module_id INTEGER NOT NULL,
                    original_name TEXT NOT NULL,
                    semantic_name TEXT NOT NULL,
                    origin TEXT NOT NULL,
                    accepted INTEGER NOT NULL,
                    evidence TEXT,
                    gate_status TEXT NOT NULL
                );
                CREATE TABLE semantic_binding_names (
                    project_id INTEGER NOT NULL,
                    file_path TEXT NOT NULL,
                    original_name TEXT NOT NULL,
                    binding_index INTEGER,
                    binding_key TEXT NOT NULL,
                    semantic_name TEXT NOT NULL,
                    origin TEXT NOT NULL,
                    evidence TEXT,
                    accepted INTEGER NOT NULL,
                    gate_status TEXT NOT NULL
                );
                INSERT INTO project_files (project_id, file_id) VALUES (1, 100);
                INSERT INTO modules (id, file_id) VALUES (10, 100);
                ",
            )
            .expect("create gate schema");
        drop(connection);
        (temp, path)
    }

    #[test]
    fn generate_project_rejects_active_symbol_name_without_gate_passed_proposal() {
        let (_temp, path) = gate_db();
        let connection = Connection::open(path.as_path()).expect("open sqlite");
        connection
            .execute(
                r"
                INSERT INTO symbols (
                    module_id, original_name, semantic_name, semantic_name_source,
                    export_name, scope_level
                ) VALUES (10, '$F1', 'createClient', 'agent', NULL, 'module')
                ",
                [],
            )
            .expect("insert symbol");
        drop(connection);

        let error = validate_accepted_naming_gate_records(path.as_path(), 1)
            .expect_err("ungated active symbol names must be rejected");
        assert!(
            error
                .to_string()
                .contains("no matching accepted gate-passed proposal")
        );
    }

    #[test]
    fn generate_project_grandfathers_legacy_symbol_name_without_source() {
        // A legacy project: an active module symbol name with NO
        // `semantic_name_source` (applied before the proposal-provenance system).
        // There is no proposal table for it and none can match. The gate must
        // grandfather it (allow generation) rather than hard-block, otherwise every
        // pre-gate project becomes ungeneratable.
        let (_temp, path) = gate_db();
        let connection = Connection::open(path.as_path()).expect("open sqlite");
        // Drop the proposals table to mimic a genuinely legacy DB.
        connection
            .execute("DROP TABLE symbol_name_proposals", [])
            .expect("drop proposals");
        connection
            .execute(
                r"
                INSERT INTO symbols (
                    module_id, original_name, semantic_name, semantic_name_source,
                    export_name, scope_level
                ) VALUES (10, '$F1', 'createClient', NULL, NULL, 'module')
                ",
                [],
            )
            .expect("insert symbol");
        drop(connection);

        validate_accepted_naming_gate_records(path.as_path(), 1)
            .expect("legacy un-provenanced names must be grandfathered, not blocked");
    }

    #[test]
    fn generate_project_revalidates_gate_passed_symbol_proposal_content() {
        let (_temp, path) = gate_db();
        let connection = Connection::open(path.as_path()).expect("open sqlite");
        connection
            .execute(
                r"
                INSERT INTO symbols (
                    module_id, original_name, semantic_name, semantic_name_source,
                    export_name, scope_level
                ) VALUES (10, '$F1', 'billingInvoiceHandler', 'agent', NULL, 'module')
                ",
                [],
            )
            .expect("insert symbol");
        connection
            .execute(
                r"
                INSERT INTO symbol_name_proposals (
                    project_id, module_id, original_name, semantic_name, origin,
                    accepted, evidence, gate_status
                ) VALUES (1, 10, '$F1', 'billingInvoiceHandler', 'agent',
                          1, 'route:/api/session handler', 'passed')
                ",
                [],
            )
            .expect("insert proposal");
        drop(connection);

        let error = validate_accepted_naming_gate_records(path.as_path(), 1)
            .expect_err("passed status is not enough if deterministic gates fail");
        assert!(error.to_string().contains("absent from evidence"));
    }

    #[test]
    fn generate_project_rejects_accepted_binding_name_without_passed_gate() {
        let (_temp, path) = gate_db();
        let connection = Connection::open(path.as_path()).expect("open sqlite");
        connection
            .execute(
                r"
                INSERT INTO semantic_binding_names (
                    project_id, file_path, original_name, binding_index, binding_key,
                    semantic_name, origin, evidence, accepted, gate_status
                ) VALUES (1, 'src/index.ts', 'a', NULL, '', 'refreshAccessToken',
                          'agent', 'string:refresh_token string:access_token', 1, 'legacy')
                ",
                [],
            )
            .expect("insert binding name");
        drop(connection);

        let error = validate_accepted_naming_gate_records(path.as_path(), 1)
            .expect_err("accepted binding names require passed gates");
        assert!(error.to_string().contains("expected passed"));
    }

    #[test]
    fn generate_project_accepts_binding_name_with_passed_revalidated_gate() {
        let (_temp, path) = gate_db();
        let connection = Connection::open(path.as_path()).expect("open sqlite");
        connection
            .execute(
                r"
                INSERT INTO semantic_binding_names (
                    project_id, file_path, original_name, binding_index, binding_key,
                    semantic_name, origin, evidence, accepted, gate_status
                ) VALUES (?1, 'src/index.ts', 'a', NULL, '', 'refreshAccessToken',
                          'agent', 'string:refresh_token string:access_token', 1, 'passed')
                ",
                params![1_i64],
            )
            .expect("insert binding name");
        drop(connection);

        validate_accepted_naming_gate_records(path.as_path(), 1)
            .expect("gate-passed accepted binding name should pass preflight");
    }

    // --- Source-restoration (recognized-package sidecar) coverage ---------------

    /// A fixture DB with the `package_attributions` and `package_source_cache`
    /// schemas used by the restoration writer, matching production columns.
    fn restoration_db() -> (TempDir, std::path::PathBuf) {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("input.sqlite");
        let connection = Connection::open(path.as_path()).expect("open sqlite");
        connection
            .execute_batch(
                r"
                CREATE TABLE package_attributions (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    module_id INTEGER NOT NULL,
                    module_original_name TEXT NOT NULL,
                    package_name TEXT NOT NULL,
                    package_version TEXT NOT NULL,
                    package_subpath TEXT,
                    resolved_file TEXT,
                    export_specifier TEXT,
                    emission_mode TEXT NOT NULL,
                    status TEXT NOT NULL,
                    evidence_json TEXT,
                    rejection_reason TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    UNIQUE (module_id)
                );
                CREATE TABLE package_source_cache (
                    package_name TEXT NOT NULL,
                    package_version TEXT NOT NULL,
                    entry_path TEXT NOT NULL,
                    source_content TEXT NOT NULL,
                    content_hash TEXT NOT NULL,
                    external_importable INTEGER NOT NULL DEFAULT 1,
                    external_import_policy_version INTEGER NOT NULL DEFAULT 0,
                    export_specifier TEXT NOT NULL DEFAULT '',
                    fetched_at TEXT NOT NULL,
                    expires_at TEXT NOT NULL,
                    PRIMARY KEY (package_name, package_version, entry_path)
                );
                ",
            )
            .expect("create restoration schema");
        drop(connection);
        (temp, path)
    }

    fn insert_attribution(
        connection: &Connection,
        module_id: i64,
        package: &str,
        version: &str,
        subpath: Option<&str>,
        status: &str,
    ) {
        connection
            .execute(
                r"
                INSERT INTO package_attributions (
                    module_id, module_original_name, package_name, package_version,
                    package_subpath, emission_mode, status, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, 'inline', ?6, 't', 't')
                ",
                params![
                    module_id,
                    format!("m{module_id}"),
                    package,
                    version,
                    subpath,
                    status
                ],
            )
            .expect("insert attribution");
    }

    fn insert_cache_entry(
        connection: &Connection,
        package: &str,
        version: &str,
        entry_path: &str,
        source: &str,
    ) {
        connection
            .execute(
                r"
                INSERT INTO package_source_cache (
                    package_name, package_version, entry_path, source_content,
                    content_hash, export_specifier, fetched_at, expires_at
                ) VALUES (?1, ?2, ?3, ?4, 'h', '', 't', 't')
                ",
                params![package, version, entry_path, source],
            )
            .expect("insert cache entry");
    }

    fn read_manifest(metadata_dir: &std::path::Path) -> Vec<serde_json::Value> {
        let raw = std::fs::read_to_string(metadata_dir.join("recognized-packages.json"))
            .expect("manifest written");
        match serde_json::from_str(&raw).expect("manifest is valid json") {
            serde_json::Value::Array(items) => items,
            other => panic!("manifest must be an array, got {other}"),
        }
    }

    #[test]
    fn sanitize_restored_path_namespaces_scope_version_and_subpath() {
        assert_eq!(
            sanitize_restored_path("@sentry/electron", "4.5.0", "main/index.js"),
            "@sentry/electron@4.5.0/main/index.js"
        );
        // Empty subpath falls back to index.js.
        assert_eq!(
            sanitize_restored_path("semver", "7.6.0", ""),
            "semver@7.6.0/index.js"
        );
        // A leading slash on the subpath is trimmed so the path stays relative.
        assert_eq!(
            sanitize_restored_path("ws", "8.0.0", "/lib/websocket.js"),
            "ws@8.0.0/lib/websocket.js"
        );
        // Characters outside the allowlist are replaced with '-' (no spaces/colons).
        assert_eq!(
            sanitize_restored_path("pkg", "1.0.0", "a b:c.js"),
            "pkg@1.0.0/a-b-c.js"
        );
    }

    #[test]
    fn restored_source_for_prefers_exact_then_falls_back_to_basename() {
        let (_temp, path) = restoration_db();
        let connection =
            Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("open ro");

        // Exact entry_path match.
        {
            let connection = Connection::open(&path).expect("open rw");
            insert_cache_entry(&connection, "semver", "7.6.0", "index.js", "// barrel");
            insert_cache_entry(
                &connection,
                "semver",
                "7.6.0",
                "classes/range.js",
                "// real range",
            );
            // A submodule the bundle keys under a build prefix the bare subpath misses.
            insert_cache_entry(&connection, "ws", "8.0.0", "lib/websocket.js", "// ws impl");
        }

        // Empty subpath resolves to index.js.
        assert_eq!(
            restored_source_for(&connection, "semver", "7.6.0", "")
                .expect("query ok")
                .as_deref(),
            Some("// barrel")
        );
        // Exact subpath hit.
        assert_eq!(
            restored_source_for(&connection, "semver", "7.6.0", "classes/range.js")
                .expect("query ok")
                .as_deref(),
            Some("// real range")
        );
        // Basename fallback: subpath `websocket.js` keyed under `lib/websocket.js`.
        assert_eq!(
            restored_source_for(&connection, "ws", "8.0.0", "websocket.js")
                .expect("query ok")
                .as_deref(),
            Some("// ws impl")
        );
        // A genuine miss returns None, not an error.
        assert_eq!(
            restored_source_for(&connection, "semver", "7.6.0", "does/not/exist.js")
                .expect("query ok"),
            None
        );
    }

    #[test]
    fn recognized_manifest_distinguishes_externalized_from_inlined() {
        let (temp, path) = restoration_db();
        {
            let connection = Connection::open(&path).expect("open rw");
            // An accepted (externalized) module and a rejected-but-matched (inlined) one.
            insert_attribution(&connection, 10, "semver", "7.6.0", None, "accepted");
            insert_attribution(
                &connection,
                11,
                "@sentry/electron",
                "4.5.0",
                Some("main/index.js"),
                "rejected",
            );
        }
        let metadata_dir = temp.path().join(".reverts");
        std::fs::create_dir_all(&metadata_dir).expect("metadata dir");

        let mut module_output_paths = BTreeMap::new();
        module_output_paths.insert(reverts_ir::ModuleId(11), "src/sentry-main.ts".to_string());

        write_recognized_package_sources(&path, &module_output_paths, &metadata_dir)
            .expect("restoration writer ok");

        let manifest = read_manifest(&metadata_dir);
        assert_eq!(manifest.len(), 2);

        let semver = manifest
            .iter()
            .find(|item| item["package"] == "semver")
            .expect("semver in manifest");
        assert_eq!(semver["externalized"], serde_json::Value::Bool(true));
        assert_eq!(semver["status"], "externalized");
        // No output path was supplied for an externalized/inlined-island module.
        assert_eq!(semver["file_path"], serde_json::Value::Null);

        let sentry = manifest
            .iter()
            .find(|item| item["package"] == "@sentry/electron")
            .expect("sentry in manifest");
        assert_eq!(sentry["externalized"], serde_json::Value::Bool(false));
        assert_eq!(sentry["status"], "rejected");
        assert_eq!(sentry["subpath"], "main/index.js");
        assert_eq!(sentry["file_path"], "src/sentry-main.ts");
        // M2 annotation: the package-aware vendored path for each recognized module.
        assert_eq!(sentry["vendor_path"], "vendor/sentry/electron/main/index.ts");
        assert_eq!(semver["vendor_path"], "vendor/semver/index.ts");
    }

    #[test]
    fn recognized_restoration_dumps_full_package_source_tree() {
        let (temp, path) = restoration_db();
        {
            let connection = Connection::open(&path).expect("open rw");
            // Only ONE module is attributed (NULL subpath, as island-inlined packages
            // are), yet the whole cached source tree should still be restored.
            insert_attribution(&connection, 10, "semver", "7.6.0", None, "rejected");
            insert_cache_entry(&connection, "semver", "7.6.0", "index.js", "// barrel");
            insert_cache_entry(
                &connection,
                "semver",
                "7.6.0",
                "classes/range.js",
                "// range",
            );
            insert_cache_entry(
                &connection,
                "semver",
                "7.6.0",
                "functions/compare.js",
                "// compare",
            );
            // A different package version must not be pulled into this tree.
            insert_cache_entry(&connection, "semver", "9.9.9", "index.js", "// wrong ver");
        }
        let metadata_dir = temp.path().join(".reverts");
        std::fs::create_dir_all(&metadata_dir).expect("metadata dir");

        let restored =
            write_recognized_package_sources(&path, &BTreeMap::new(), &metadata_dir)
                .expect("restoration writer ok");
        assert_eq!(restored, 3, "all three 7.6.0 entries restored, not the 9.9.9 one");

        let base = metadata_dir.join("restored-sources").join("semver@7.6.0");
        assert_eq!(
            std::fs::read_to_string(base.join("index.js")).expect("index restored"),
            "// barrel"
        );
        assert_eq!(
            std::fs::read_to_string(base.join("classes/range.js")).expect("range restored"),
            "// range"
        );
        assert_eq!(
            std::fs::read_to_string(base.join("functions/compare.js")).expect("compare restored"),
            "// compare"
        );
        assert!(
            !base.join("../semver@9.9.9").join("index.js").exists(),
            "the unrecognized 9.9.9 version must not be restored"
        );
    }

    /// Mirror of `output_paths::is_safe_typescript_module_path` (crate-private in
    /// reverts-pipeline): a path the output layout emits verbatim rather than
    /// slugging into `modules/<id>-…`.
    fn is_safe_typescript_layout_path(path: &str) -> bool {
        (path.ends_with(".ts") || path.ends_with(".tsx"))
            && path.split('/').all(|segment| {
                !segment.is_empty()
                    && segment != "."
                    && segment != ".."
                    && segment
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
            })
    }

    #[test]
    fn vendor_module_path_maps_package_and_subpath_into_a_ts_layout_path() {
        assert_eq!(
            vendor_module_path("semver", "classes/range.js"),
            "vendor/semver/classes/range.ts"
        );
        // Scope `@` is dropped; the `/` inside the scope stays a separator.
        assert_eq!(
            vendor_module_path("@sentry/electron", "main/index.js"),
            "vendor/sentry/electron/main/index.ts"
        );
        // A NULL/empty subpath (the common island-inlined case) → index.ts.
        assert_eq!(vendor_module_path("semver", ""), "vendor/semver/index.ts");
        // A non-source extension (e.g. `.json`) is preserved as a slugged segment,
        // not stripped, and the whole path still ends in `.ts`.
        assert_eq!(
            vendor_module_path("pkg", "data/table.json"),
            "vendor/pkg/data/table.json.ts"
        );
    }

    #[test]
    fn vendor_module_path_output_is_always_gate_and_layout_safe() {
        // Adversarial inputs: traversal, absolute, empty segments, odd chars.
        let cases = [
            ("@scope/../evil", "../../etc/passwd"),
            ("", "/leading/slash/x.js"),
            ("a b:c", "weird name!.js"),
            ("...", "./.."),
            ("pkg", ""),
            ("@only-scope", "a//b///c.js"),
        ];
        for (package, subpath) in cases {
            let path = vendor_module_path(package, subpath);
            // The output layout emits it verbatim (no `modules/<id>-…` slug fallback).
            assert!(
                is_safe_typescript_layout_path(&path),
                "layout-unsafe path {path:?} from ({package:?}, {subpath:?})"
            );
            // And the module-path naming gate accepts it.
            validate_module_path_acceptance(&path, "package-attribution").unwrap_or_else(|err| {
                panic!("gate rejected {path:?} from ({package:?}, {subpath:?}): {err:?}")
            });
            assert!(path.starts_with("vendor/"));
            assert!(!path.contains("//"), "collapsed separators in {path:?}");
        }
    }

    #[test]
    fn recognized_restoration_writes_empty_manifest_without_attributions_table() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("input.sqlite");
        // A DB with no package_attributions table at all.
        Connection::open(&path).expect("open sqlite");
        let metadata_dir = temp.path().join(".reverts");
        std::fs::create_dir_all(&metadata_dir).expect("metadata dir");

        let restored =
            write_recognized_package_sources(&path, &BTreeMap::new(), &metadata_dir)
                .expect("restoration writer tolerates a missing table");
        assert_eq!(restored, 0);
        assert_eq!(
            std::fs::read_to_string(metadata_dir.join("recognized-packages.json"))
                .expect("manifest written"),
            "[]\n"
        );
    }
}
