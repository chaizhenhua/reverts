//! `match-packages` command use-case orchestration.
//!
//! The root CLI module keeps argument parsing and public API wrappers; this
//! module owns the DB-backed matching workflow so package-source/cache matching
//! does not keep growing the command facade.

use std::time::Instant;

use reverts_input::sqlite::load_project_rows_from_connection;
use reverts_ir::ModuleKind;
use reverts_observe::AuditReport;
use reverts_package_matcher::match_packages_with_pipeline;
use reverts_pipeline::prepare_input_rows_for_pipeline_with_reserved_ids;
use rusqlite::Connection;

use crate::args::MatchPackagesArgs;
use crate::commands::package_surface_decisions::{
    reconcile_cache_surfaces_after_attribution_safety, suppress_rejected_or_blocked_surfaces,
};
use crate::errors::MatchPackagesError;
use crate::persistence::attributions;
use crate::persistence::repository::{MatchPackagePersistence, SqliteMatchPackagePersistence};
use crate::{
    MatchPackagesOutcome, dedup_audit_report, enrich_package_modules_from_source_units,
    filter_package_sources_to_referenced_package_versions,
    load_package_sources_with_fingerprint_stats, package_module_source_quality_counts,
    package_names_from_reference_source_roots, package_source_load_scope,
    package_version_resolution_evidence, package_versions_by_module,
    remove_package_attributions_for_revalidation,
    resolve_package_version_hints_to_available_sources,
};

/// Largest id used by ANY module across the whole `modules` table, over both
/// the module-id and the referenced `file_id` namespaces. The synthetic-id
/// allocator must start past this so a reconstructed source cannot alias an
/// orphan module's `file_id` (orphans are invisible to the project-scoped load
/// because their `file_id` is absent from `project_files`).
fn max_module_id_space(
    connection: &Connection,
    _project_id: u32,
) -> Result<u32, MatchPackagesError> {
    let max_id: i64 = connection
        .query_row(
            "SELECT COALESCE(MAX(v), 0) FROM (
                 SELECT id AS v FROM modules
                 UNION ALL
                 SELECT file_id AS v FROM modules WHERE file_id IS NOT NULL
             )",
            [],
            |row| row.get(0),
        )
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(u32::try_from(max_id.max(0)).unwrap_or(u32::MAX))
}

pub(crate) fn match_packages_from_connection(
    connection: &mut Connection,
    args: &MatchPackagesArgs,
) -> Result<MatchPackagesOutcome, MatchPackagesError> {
    let timing_enabled = std::env::var_os("REVERTS_MATCH_TIMING").is_some();
    let timing_started = Instant::now();
    let mut timing_last = timing_started;
    macro_rules! mark_timing {
        ($stage:literal) => {
            if timing_enabled {
                let now = Instant::now();
                eprintln!(
                    "match-packages timing: {} stage={:.3}s total={:.3}s",
                    $stage,
                    now.duration_since(timing_last).as_secs_f64(),
                    now.duration_since(timing_started).as_secs_f64()
                );
                timing_last = now;
            }
        };
    }
    let mut rows = load_project_rows_from_connection(connection, args.project_id)
        .map_err(MatchPackagesError::LoadInput)?;
    mark_timing!("load_project_rows");

    // Shared bundle-aware row preparation: split recognised bundle wrappers
    // into per-module rows before either matcher or generator sees them.
    // Reserve the whole-table id space so freshly reconstructed synthetic
    // sources cannot alias an orphan module's file_id (a legacy reconstruction
    // dropped from the load by the project_files filter) and resurrect it
    // against a mismatched span when persisted.
    let reserved_max_id = max_module_id_space(connection, args.project_id)?;
    let prepared = prepare_input_rows_for_pipeline_with_reserved_ids(rows, reserved_max_id);
    let extraction_audit = prepared.audit;
    // Snapshot new_modules from the shared preparation — we need them later
    // to persist synthetic rows into the SQLite modules table so
    // function-level attributions can FK them.
    let synthetic_modules = prepared.synthetic_modules;
    rows = prepared.rows;
    enrich_package_modules_from_source_units(connection, &mut rows, args.project_id)?;
    mark_timing!("bundle_extract_enrich");

    let mut source_import_audit = AuditReport::default();
    let reference_package_names = package_names_from_reference_source_roots(
        &args.reference_source_roots,
        &mut source_import_audit,
    )?;
    if !args.reference_source_roots.is_empty() {
        let preview = reference_package_names
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "match-packages: discovered {} reference-source package candidate(s){}{}",
            reference_package_names.len(),
            if preview.is_empty() { "" } else { ": " },
            preview
        );
    }
    let package_names = if args.package_names.is_empty() {
        let mut package_names = package_source_load_scope(&rows, &[], &mut source_import_audit);
        package_names.extend(reference_package_names);
        package_names
    } else {
        let mut requested_package_names = args.package_names.clone();
        requested_package_names.extend(reference_package_names);
        package_source_load_scope(&rows, &requested_package_names, &mut source_import_audit)
    };
    let package_filter = (!args.package_names.is_empty()).then_some(&package_names);
    remove_package_attributions_for_revalidation(&mut rows, &package_names);
    let loaded_package_sources = load_package_sources_with_fingerprint_stats(
        connection,
        &rows,
        &package_names,
        &args.package_source_roots,
        args.materialize_package_sources,
        args.apply,
    )?;
    let fingerprint_cache = loaded_package_sources.fingerprint_cache;
    let mut package_sources = loaded_package_sources.sources;
    mark_timing!("load_package_sources");
    let package_versions_before_resolution = package_versions_by_module(&rows);
    resolve_package_version_hints_to_available_sources(
        &mut rows,
        &package_sources,
        &package_names,
    )?;
    let package_version_resolutions =
        package_version_resolution_evidence(&package_versions_before_resolution, &rows);
    mark_timing!("resolve_versions");
    filter_package_sources_to_referenced_package_versions(&rows, &mut package_sources);
    mark_timing!("filter_referenced_versions");
    let source_quality_counts = package_module_source_quality_counts(&rows, package_filter);
    mark_timing!("source_quality_counts");
    let pipeline_report = match_packages_with_pipeline(&rows, &package_sources, package_filter);
    mark_timing!("match_pipeline");
    let mut report = pipeline_report.package_report;
    report.audit.extend(source_import_audit);
    suppress_rejected_or_blocked_surfaces(connection, args.project_id, &mut report)?;
    let external_import_candidates = report.attributions.len();
    let external_import_safety =
        attributions::filter_unsafe_interpackage_external_attributions(&rows, &mut report);
    reconcile_cache_surfaces_after_attribution_safety(&rows, &mut report);
    let function_attributions = pipeline_report.function_attributions;
    let function_ownership_matches = pipeline_report.function_ownership_matches;
    let persistence_package_names = if package_names.is_empty() {
        report
            .matches
            .iter()
            .map(|package_match| package_match.package_name.clone())
            .collect()
    } else {
        package_names.clone()
    };

    let (written_attributions, written_surfaces, written_function_attributions) = if args.apply {
        let mut persistence = SqliteMatchPackagePersistence::new(connection);
        let outcome = persistence.persist_match_package_outputs(
            &rows,
            &synthetic_modules,
            &report,
            &persistence_package_names,
            &package_version_resolutions,
            &function_attributions,
        )?;
        (
            outcome.written_attributions,
            outcome.written_surfaces,
            outcome.written_function_attributions,
        )
    } else {
        (0, 0, 0)
    };
    mark_timing!("persist");
    if timing_enabled {
        let _ = timing_last;
    }

    let matched_modules = report.matches.len();
    let loaded_package_modules = rows
        .modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
        .count();
    let source_elimination = attributions::package_source_elimination_stats_for_report(
        &rows,
        &report,
        loaded_package_modules,
    );
    let matched_package_surfaces = report.surfaces.len();
    let mut audit = extraction_audit;
    audit.extend(report.audit);
    let audit = dedup_audit_report(audit);

    Ok(MatchPackagesOutcome {
        project_id: args.project_id,
        loaded_package_modules,
        loaded_package_sources: package_sources.len(),
        fingerprint_cache_hits: fingerprint_cache.cache_hits,
        fingerprint_cache_misses: fingerprint_cache.cache_misses,
        fingerprint_cache_computed: fingerprint_cache.computed,
        fingerprint_cache_errors: fingerprint_cache.total_errors(),
        matched_modules,
        external_import_modules: source_elimination.direct_external_import_modules,
        private_source_suppressed_package_modules: source_elimination
            .private_source_suppressed_package_modules,
        source_eliminated_package_modules: source_elimination.source_eliminated_package_modules,
        remaining_package_source_modules: source_elimination.remaining_package_source_modules,
        external_import_candidates,
        unsafe_external_import_modules: external_import_safety.removed_modules,
        matched_package_surfaces,
        written_attributions,
        written_surfaces,
        function_attributions: function_attributions.len(),
        function_ownership_matches,
        written_function_attributions,
        package_source_quality_trusted: source_quality_counts.trusted,
        package_source_quality_weak: source_quality_counts.weak,
        package_source_quality_invalid: source_quality_counts.invalid,
        package_source_quality_missing: source_quality_counts.missing,
        audit,
        external_import_blockers: external_import_safety.blockers,
    })
}
