//! `match-packages` command use-case orchestration.
//!
//! The root CLI module keeps argument parsing and public API wrappers; this
//! module owns the DB-backed matching workflow so package-source/cache matching
//! does not keep growing the command facade.

use std::collections::BTreeSet;
use std::time::Instant;

use reverts_input::sqlite::load_project_rows_from_connection;
use reverts_input::{InputBundle, InputRows};
use reverts_ir::ModuleKind;
use reverts_model::ProgramModel;
use reverts_observe::AuditReport;
use reverts_package_matcher::{
    PackageSource, anchor_island_cjs_modules, anchor_prelude_bindings,
    match_packages_with_pipeline, prelude_binding_sources,
};
use reverts_pipeline::prepare_input_rows_for_pipeline_with_reserved_ids;
use rusqlite::Connection;

use crate::args::MatchPackagesArgs;
use crate::commands::package_surface_decisions::{
    reconcile_cache_surfaces_after_attribution_safety, suppress_rejected_or_blocked_surfaces,
};
use crate::errors::MatchPackagesError;
use crate::persistence::attributions;
use crate::persistence::island_anchors::{IslandPackageAnchor, persist_island_anchors};
use crate::persistence::island_package_candidates::load_accepted_island_package_candidates;
use crate::persistence::repository::{MatchPackagePersistence, SqliteMatchPackagePersistence};
use crate::{
    MatchPackagesOutcome, dedup_audit_report, enrich_package_modules_from_source_units,
    filter_package_sources_to_referenced_package_versions,
    load_package_sources_with_fingerprint_stats, package_module_source_quality_counts,
    package_names_and_versions_from_reference_source_roots, package_source_load_scope,
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
    let (mut reference_package_names, mut reference_package_versions) =
        package_names_and_versions_from_reference_source_roots(
            &args.reference_source_roots,
            &mut source_import_audit,
        )?;
    // Agent-proposed island package names (deterministically confirmed downstream
    // by the fingerprint cascade) seed materialization exactly like reference
    // dependencies — this is how inlined libraries with no bundle module and no
    // reference manifest entry get a corpus to match against.
    let agent_island_candidates =
        load_accepted_island_package_candidates(connection, i64::from(args.project_id))?;
    if !agent_island_candidates.is_empty() {
        eprintln!(
            "match-packages: seeding {} Agent-proposed island package name(s)",
            agent_island_candidates.len()
        );
        for candidate in &agent_island_candidates {
            reference_package_names.insert(candidate.package_name.clone());
            if let Some(version) = &candidate.version_hint {
                reference_package_versions
                    .entry(candidate.package_name.clone())
                    .or_insert_with(|| version.clone());
            }
        }
    }
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
    // The corpus to load = packages a bundle module references (vendored matching
    // targets) + packages an Agent nominated as inlined island libraries. The full
    // reference `package.json` dependency set also contains build/dev tooling and
    // asset packages with NO bundle module and NO island candidacy (on a real
    // Electron app `@phosphor-icons/react` alone is >4k sources, >50% of the
    // corpus). Those can never match a bundle module or an island unit, so loading
    // them only inflates every fingerprinting pass (and pushes the source count
    // past the cascade limit). Restrict the blanket reference extension to the
    // island candidate names; bundle-referenced reference packages still come in
    // via `package_source_load_scope`, and reference *versions* stay available for
    // resolution regardless.
    let island_candidate_names: BTreeSet<String> = agent_island_candidates
        .iter()
        .map(|candidate| candidate.package_name.clone())
        .collect();
    let package_names = if args.package_names.is_empty() {
        let mut package_names = package_source_load_scope(&rows, &[], &mut source_import_audit);
        package_names.extend(island_candidate_names.iter().cloned());
        package_names
    } else {
        let mut requested_package_names = args.package_names.clone();
        requested_package_names.extend(island_candidate_names.iter().cloned());
        package_source_load_scope(&rows, &requested_package_names, &mut source_import_audit)
    };
    let package_filter = (!args.package_names.is_empty()).then_some(&package_names);
    // Snapshot the prepared rows BEFORE revalidation strips package attributions:
    // the island model must rebuild from rows that still pass `from_rows`
    // validation (every package module keeps its attribution). The eager
    // preludes the island anchoring needs do not depend on attributions or
    // resolved versions, so this pre-mutation snapshot is the right input.
    let island_rows = rows.clone();
    remove_package_attributions_for_revalidation(&mut rows, &package_names);
    let loaded_package_sources = load_package_sources_with_fingerprint_stats(
        connection,
        &rows,
        &package_names,
        &args.package_source_roots,
        &reference_package_versions,
        args.materialize_package_sources,
        args.apply,
    )?;
    let fingerprint_cache = loaded_package_sources.fingerprint_cache;
    let island_corpus = loaded_package_sources.island_corpus;
    let island_corpus_len = island_corpus.len();
    eprintln!("match-packages: island corpus has {island_corpus_len} no-module library source(s)");
    let mut package_sources = loaded_package_sources.sources;
    // Island-candidate packages are INLINED libraries with no bundle module of
    // their own — they externalize through island anchoring (`island_corpus`),
    // never through the main vendored-module cascade. Keeping their sources in the
    // main `package_sources` only makes the cascade match all bundle modules
    // against them (a generic package like `@opentelemetry/sdk-trace-base` triggers
    // a candidate explosion → ~80s of Hungarian work for zero useful matches).
    // Drop them here; `island_corpus` retains them for anchoring. A package that is
    // genuinely BOTH a bundle module and a candidate keeps its bundle-module rows,
    // so its vendored match is unaffected.
    if !island_candidate_names.is_empty() {
        let bundle_module_packages: BTreeSet<String> = rows
            .modules
            .iter()
            .filter_map(|module| module.package_name.as_deref())
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect();
        let before = package_sources.len();
        package_sources.retain(|source| {
            !island_candidate_names.contains(&source.package_name)
                || bundle_module_packages.contains(&source.package_name)
        });
        if package_sources.len() != before {
            eprintln!(
                "match-packages: excluded {} inlined island-candidate source(s) from the vendored cascade (kept for island anchoring)",
                before - package_sources.len()
            );
        }
    }
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
    // Recover bundled libraries flattened into the scope-hoisted eager entry
    // island. They never become model modules, so the per-module pipeline above
    // never sees them; this anchors them per-binding against the same package
    // corpus. Skipped when there is no corpus to match against.
    //
    // Honor `--package-name`: like the cascade pass, restrict the island corpus to
    // the requested packages so an incremental re-match anchors the island against
    // ONLY those sources. The full corpus is hundreds of no-module library sources;
    // matching every island CJS unit against all of them is the second per-run
    // bottleneck (tens of minutes) that gates incremental externalization.
    // Island anchoring is O(island_units × corpus). The full reference corpus is
    // dominated by packages that can NEVER be inlined into the runtime main-process
    // island — icon sets, build/dev tooling, test runners, type-only packages (on a
    // real Electron app `@phosphor-icons/react` alone is >50% of the corpus). Scope
    // the corpus to the packages an Agent actually nominated as inlined
    // (`island_package_candidates`), which is exactly the set island anchoring can
    // confirm. An explicit `--package-name` narrows further (incremental runs);
    // with neither signal we fall back to the full corpus (pure discovery).
    let requested_island_packages: BTreeSet<String> = args.package_names.iter().cloned().collect();
    let island_scope: Option<&BTreeSet<String>> = if !requested_island_packages.is_empty() {
        Some(&requested_island_packages)
    } else if !island_candidate_names.is_empty() {
        Some(&island_candidate_names)
    } else {
        None
    };
    let scoped_island_corpus: Vec<PackageSource> = match island_scope {
        None => island_corpus,
        Some(scope) => island_corpus
            .iter()
            .filter(|source| scope.contains(&source.package_name))
            .cloned()
            .collect(),
    };
    let island_scope_origin = if !requested_island_packages.is_empty() {
        "--package-name"
    } else if !island_candidate_names.is_empty() {
        "accepted island candidates"
    } else {
        "full corpus (no candidates)"
    };
    eprintln!(
        "match-packages: island anchoring corpus scoped to {} source(s) from {} (was {} total; via {})",
        scoped_island_corpus.len(),
        island_scope.map_or(0, BTreeSet::len),
        island_corpus_len,
        island_scope_origin,
    );
    let island_anchors = compute_island_anchors(island_rows, &scoped_island_corpus);
    if !island_anchors.is_empty() {
        eprintln!(
            "match-packages: anchored {} eager entry-island binding(s) to packages",
            island_anchors.len()
        );
    }
    mark_timing!("island_anchor");
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
    if args.apply {
        persist_island_anchors(connection, i64::from(args.project_id), &island_anchors)?;
    }
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

/// Anchor the eager entry-island bindings of a scope-hoisted bundle to package
/// sources.
///
/// The per-module matcher above only sees model modules; a scope-hoisting
/// bundler flattens its eagerly-evaluated modules (including bundled libraries)
/// into one top-level scope with no module of their own, so they are invisible
/// to it. Here we rebuild the program model purely to reach the runtime
/// preludes, enumerate their eager (`SourceBacked`) bindings, and anchor each
/// against the same `package_sources` corpus via the function-ownership
/// cascade. Returns one row per anchored binding, keyed by the prelude's
/// `source_file_id` so the generate stage can join them back.
///
/// With no corpus there is nothing to match against, so the (non-trivial) model
/// rebuild is skipped.
fn compute_island_anchors(
    rows: InputRows,
    package_sources: &[PackageSource],
) -> Vec<IslandPackageAnchor> {
    if package_sources.is_empty() {
        return Vec::new();
    }
    let input = match InputBundle::from_rows(rows) {
        Ok(input) => input,
        Err(error) => {
            eprintln!(
                "match-packages: island anchoring skipped — could not rebuild program model: {error}"
            );
            return Vec::new();
        }
    };
    let model = ProgramModel::from_input(input);

    // Per-binding anchoring fingerprints EVERY eager island binding (tens of
    // thousands on a real bundle) as its own cascade subject — prohibitively
    // slow, and most subjects are trivial exports objects / guards / genuine
    // application code that can never match a package. It is an opt-in deep
    // scan; the default is the module-unit pass below.
    let deep_per_binding = std::env::var("REVERTS_ISLAND_DEEP_ANCHOR").is_ok();

    let mut anchors: Vec<IslandPackageAnchor> = Vec::new();
    let mut total_units = 0_usize;
    let mut total_bindings = 0_usize;
    for (source_file_id, prelude) in model.graph().runtime_preludes() {
        // Fast path: recognize inlined CommonJS module units structurally and
        // match each unit's INIT body once, at module granularity.
        let unit_anchors = anchor_island_cjs_modules(prelude, package_sources);
        total_units += unit_anchors.len();
        let mut anchored_bindings: BTreeSet<String> = unit_anchors
            .iter()
            .map(|anchor| anchor.binding.clone())
            .collect();
        for anchor in unit_anchors {
            anchors.push(island_anchor_row(*source_file_id, anchor));
        }

        if deep_per_binding {
            let binding_sources = prelude_binding_sources(prelude);
            total_bindings += binding_sources.len();
            for anchor in anchor_prelude_bindings(&binding_sources, package_sources) {
                // The unit pass already owns this binding — don't double-anchor.
                if !anchored_bindings.insert(anchor.binding.clone()) {
                    continue;
                }
                anchors.push(island_anchor_row(*source_file_id, anchor));
            }
        }
    }
    if deep_per_binding {
        eprintln!(
            "match-packages: island anchoring matched {total_units} CJS module-unit binding(s) + deep-scanned {total_bindings} eager binding(s) against {} library source(s) -> {} anchor(s)",
            package_sources.len(),
            anchors.len()
        );
    } else {
        eprintln!(
            "match-packages: island anchoring matched {total_units} CJS module-unit binding(s) against {} library source(s) -> {} anchor(s) (set REVERTS_ISLAND_DEEP_ANCHOR=1 for the slow per-binding scan)",
            package_sources.len(),
            anchors.len()
        );
    }
    anchors
}

/// Build a persisted anchor row from a recovered prelude binding anchor.
fn island_anchor_row(
    source_file_id: u32,
    anchor: reverts_package_matcher::PreludeBindingAnchor,
) -> IslandPackageAnchor {
    IslandPackageAnchor {
        source_file_id,
        binding_name: anchor.binding,
        package_name: anchor.package_name,
        package_version: anchor.package_version,
        export_specifier: anchor.export_specifier,
        function_span_start: anchor.function_span.start,
        function_span_end: anchor.function_span.end,
        tier: anchor.confidence.tier.as_str().to_string(),
        external_importable: anchor.external_importable,
        top_score: anchor.confidence.top_score,
        runner_up_score: anchor.confidence.runner_up_score,
        margin: anchor.confidence.margin,
    }
}
