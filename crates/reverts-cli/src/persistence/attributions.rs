//! Package attribution persistence + the supporting external-import
//! safety analysis, rejection-reason diagnostics, and externalization
//! chain proofs.
//!
//! `match_packages_from_connection` calls into this module once per
//! project to (1) collapse the accepted [`VersionedPackageMatchReport`]
//! attributions, (2) compute and persist rejected attributions for
//! unmatched package modules, and (3) filter unsafe inter-package
//! external import attributions before they reach `package_attributions`.

use std::collections::{BTreeMap, BTreeSet};

use reverts_graph::FunctionExtractor;
use reverts_input::{
    InputRows, ModuleDependencyTarget, ModuleInput,
    PACKAGE_ATTRIBUTION_EXTERNAL_IMPORT_POLICY_VERSION, PackageAttributionInput,
    PackageAttributionStatus, PackageEmissionMode,
};
use reverts_ir::{ModuleId, ModuleKind};
use reverts_package::{
    ConsumerBoundaryPolicy, consumer_is_boundary, external_import_proof_label,
    is_accepted_external_attribution, same_package_consumer,
};
use reverts_package_matcher::{
    BestVersionMatch, ModuleMatchStrategy, PackageMatch, PackageModuleSourceQuality,
    VersionMatchScore, VersionedPackageMatchReport, ownership_by_module,
    package_module_source_quality,
};
use rusqlite::{Connection, OptionalExtension, params};

use crate::errors::MatchPackagesError;
use crate::sqlite_table_has_column;
use crate::{
    ExternalImportBlockerSummary, ExternalImportSafetyReport, PackageVersionResolutionEvidence,
};

pub(crate) fn persist_package_attributions(
    connection: &mut Connection,
    rows: &InputRows,
    report: &VersionedPackageMatchReport,
    matched_package_names: &BTreeSet<String>,
    version_resolutions: &BTreeMap<ModuleId, PackageVersionResolutionEvidence>,
) -> Result<usize, MatchPackagesError> {
    let rejected_attributions =
        rejected_package_attributions_for_unaccepted_modules(rows, report, matched_package_names)?;
    if report.attributions.is_empty() && rejected_attributions.is_empty() {
        return Ok(0);
    }

    ensure_package_attributions_table(connection)?;

    let matches_by_module = report
        .matches
        .iter()
        .map(|module_match| (module_match.module_id, module_match))
        .collect::<BTreeMap<_, _>>();
    let chain_proofs = externalization_chain_proofs(rows, report);
    let diagnostics_context = PackageDiagnosticsContext::new(rows, report);
    let modules_by_id = rows
        .modules
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let transaction = connection
        .transaction()
        .map_err(MatchPackagesError::WriteAttribution)?;
    let mut written = 0;

    for attribution in &report.attributions {
        let module_match = matches_by_module.get(&attribution.module_id).ok_or(
            MatchPackagesError::MissingMatchEvidence {
                module_id: attribution.module_id,
            },
        )?;
        let module = modules_by_id.get(&attribution.module_id).copied().ok_or(
            MatchPackagesError::MissingModuleForAttribution {
                module_id: attribution.module_id,
            },
        )?;
        persist_package_attribution(
            &transaction,
            module.original_name.as_str(),
            attribution,
            module_match,
            version_resolutions.get(&attribution.module_id),
            chain_proofs.get(&attribution.module_id),
        )?;
        written += 1;
    }
    for attribution in &rejected_attributions {
        let module = modules_by_id.get(&attribution.module_id).copied().ok_or(
            MatchPackagesError::MissingModuleForAttribution {
                module_id: attribution.module_id,
            },
        )?;
        persist_rejected_package_attribution(
            &transaction,
            module.original_name.as_str(),
            attribution,
            matches_by_module.get(&attribution.module_id).copied(),
            unmatched_package_diagnostics(rows, &diagnostics_context, module),
        )?;
        written += 1;
    }

    transaction
        .commit()
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(written)
}

fn ensure_package_attributions_table(
    connection: &mut Connection,
) -> Result<(), MatchPackagesError> {
    connection
        .execute_batch(PACKAGE_ATTRIBUTIONS_CREATE_SQL)
        .map_err(MatchPackagesError::WriteAttribution)?;
    if package_attributions_requires_nullable_version_migration(connection)
        .map_err(MatchPackagesError::WriteAttribution)?
    {
        migrate_package_attributions_nullable_version(connection)?;
    }
    if !sqlite_table_has_column(
        connection,
        "package_attributions",
        "external_import_policy_version",
    )
    .map_err(MatchPackagesError::WriteAttribution)?
    {
        connection
            .execute_batch(
                r"
                ALTER TABLE package_attributions
                    ADD COLUMN external_import_policy_version INTEGER NOT NULL DEFAULT 0;
                ",
            )
            .map_err(MatchPackagesError::WriteAttribution)?;
    }
    connection
        .execute_batch(PACKAGE_ATTRIBUTIONS_INDEX_SQL)
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(())
}

const PACKAGE_ATTRIBUTIONS_CREATE_SQL: &str = r"
CREATE TABLE IF NOT EXISTS package_attributions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    module_id INTEGER NOT NULL,
    module_original_name TEXT NOT NULL,
    package_name TEXT NOT NULL,
    package_version TEXT,
    package_subpath TEXT,
    resolved_file TEXT,
    export_specifier TEXT,
    emission_mode TEXT NOT NULL,
    status TEXT NOT NULL,
    evidence_json TEXT,
    rejection_reason TEXT,
    external_import_policy_version INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (module_id),
    FOREIGN KEY (module_id) REFERENCES modules(id) ON DELETE CASCADE,
    CHECK (TRIM(module_original_name) != ''),
    CHECK (TRIM(package_name) != ''),
    CHECK (emission_mode IN (
        'external_import',
        'vendored_asset',
        'application_source',
        'runtime_glue'
    )),
    CHECK (status IN ('proposed', 'accepted', 'rejected')),
    CHECK (status != 'accepted' OR TRIM(COALESCE(package_version, '')) != ''),
    CHECK (
        status != 'accepted'
        OR emission_mode != 'external_import'
        OR TRIM(COALESCE(export_specifier, '')) != ''
    ),
    CHECK (status != 'rejected' OR TRIM(COALESCE(rejection_reason, '')) != '')
);
";

const PACKAGE_ATTRIBUTIONS_INDEX_SQL: &str = r"
CREATE INDEX IF NOT EXISTS idx_package_attributions_package
    ON package_attributions(package_name, package_version);
CREATE INDEX IF NOT EXISTS idx_package_attributions_status
    ON package_attributions(status);
CREATE INDEX IF NOT EXISTS idx_package_attributions_emission
    ON package_attributions(emission_mode);
";

fn package_attributions_requires_nullable_version_migration(
    connection: &Connection,
) -> rusqlite::Result<bool> {
    let mut statement = connection.prepare("PRAGMA table_info(package_attributions)")?;
    let columns = statement.query_map([], |row| {
        Ok((row.get::<_, String>(1)?, row.get::<_, i64>(3)?))
    })?;
    let mut package_version_not_null = false;
    for column in columns {
        let (name, not_null) = column?;
        if name == "package_version" && not_null != 0 {
            package_version_not_null = true;
            break;
        }
    }

    let create_sql = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'package_attributions'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let has_legacy_non_empty_version_check = create_sql
        .as_deref()
        .is_some_and(|sql| sql.contains("TRIM(package_version) != ''"));

    Ok(package_version_not_null || has_legacy_non_empty_version_check)
}

fn migrate_package_attributions_nullable_version(
    connection: &mut Connection,
) -> Result<(), MatchPackagesError> {
    let transaction = connection
        .transaction()
        .map_err(MatchPackagesError::WriteAttribution)?;
    transaction
        .execute_batch(
            r"
            ALTER TABLE package_attributions RENAME TO package_attributions__reverts_old;
            ",
        )
        .map_err(MatchPackagesError::WriteAttribution)?;
    transaction
        .execute_batch(PACKAGE_ATTRIBUTIONS_CREATE_SQL)
        .map_err(MatchPackagesError::WriteAttribution)?;
    transaction
        .execute_batch(
            r"
            INSERT INTO package_attributions (
                id,
                module_id,
                module_original_name,
                package_name,
                package_version,
                package_subpath,
                resolved_file,
                export_specifier,
                emission_mode,
                status,
                evidence_json,
                rejection_reason,
                external_import_policy_version,
                created_at,
                updated_at
            )
            SELECT
                id,
                module_id,
                module_original_name,
                package_name,
                package_version,
                package_subpath,
                resolved_file,
                export_specifier,
                emission_mode,
                status,
                evidence_json,
                rejection_reason,
                0,
                created_at,
                updated_at
              FROM package_attributions__reverts_old;
            DROP TABLE package_attributions__reverts_old;
            ",
        )
        .map_err(MatchPackagesError::WriteAttribution)?;
    transaction
        .commit()
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(())
}

fn rejected_package_attributions_for_unaccepted_modules(
    rows: &InputRows,
    report: &VersionedPackageMatchReport,
    matched_package_names: &BTreeSet<String>,
) -> Result<Vec<PackageAttributionInput>, MatchPackagesError> {
    let accepted_modules = report
        .attributions
        .iter()
        .filter(|attribution| is_accepted_external_attribution(attribution))
        .map(|attribution| attribution.module_id)
        .chain(
            rows.package_attributions
                .iter()
                .filter(|attribution| is_accepted_external_attribution(attribution))
                .map(|attribution| attribution.module_id),
        )
        .collect::<BTreeSet<_>>();
    let decision_reasons = report
        .version_matches
        .iter()
        .map(|decision| {
            (
                decision_package_name(decision).to_string(),
                rejection_reason_from_decision(decision),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let modules_by_id = rows
        .modules
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let (outgoing_dependencies, _incoming_dependencies) = dependency_indexes(rows);

    let mut rejected = Vec::new();
    for module in &rows.modules {
        if module.kind != ModuleKind::Package || accepted_modules.contains(&module.id) {
            continue;
        }
        let package_name =
            module
                .package_name
                .as_deref()
                .ok_or(MatchPackagesError::InvalidAttribution {
                    module_id: module.id,
                    message: "package module has no package_name".to_string(),
                })?;
        if !matched_package_names.contains(package_name) {
            continue;
        }

        let match_evidence = report
            .matches
            .iter()
            .find(|package_match| package_match.module_id == module.id);
        let external_import_match =
            match_evidence.filter(|package_match| package_match.external_importable);
        let source_only_match =
            match_evidence.filter(|package_match| !package_match.external_importable);
        let reason = external_import_match
            .map(|_| {
                "matched package external import, but at least one non-externalized consumer still depends on this module"
            })
            .or_else(|| {
                source_only_match
            .filter(|package_match| {
                matches!(
                    package_match.strategy,
                    ModuleMatchStrategy::AggregateFunctionSignatureAndStringAnchors
                        | ModuleMatchStrategy::CascadeFunctionCoverage
                        | ModuleMatchStrategy::CascadeFunctionOwnership
                        | ModuleMatchStrategy::CascadePartialFunctionCoverage
                        | ModuleMatchStrategy::AggregateStructuralBagSimilarity
                        | ModuleMatchStrategy::DependencyClosureOwnership
                )
            })
            .map(|_| {
                "matched package ownership, but the evidence does not prove a safe single external import"
            })
            })
            .or_else(|| {
                package_source_quality_rejection_reason(
                    rows,
                    module,
                    package_name,
                    &modules_by_id,
                    &outgoing_dependencies,
                )
            })
            .or_else(|| decision_reasons.get(package_name).map(String::as_str))
            .unwrap_or("package matcher did not produce an accepted attribution for this package");
        let mut attribution =
            PackageAttributionInput::rejected_source(module.id, package_name, reason);
        if let Some(package_match) = match_evidence {
            attribution.package_version = Some(package_match.package_version.clone());
            if package_match.external_importable {
                attribution.export_specifier = Some(package_match.export_specifier.clone());
                attribution.resolved_file = Some(package_match.source_path.clone());
            }
        }
        rejected.push(attribution);
    }
    Ok(rejected)
}

pub(crate) fn filter_unsafe_interpackage_external_attributions(
    rows: &InputRows,
    report: &mut VersionedPackageMatchReport,
) -> ExternalImportSafetyReport {
    let modules_by_id = rows
        .modules
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let (outgoing_dependencies, incoming_dependencies) = dependency_indexes(rows);
    let ownership_proven_modules = package_ownership_proven_modules(rows, report, &modules_by_id);
    let report_external_modules = report
        .attributions
        .iter()
        .filter(|attribution| is_accepted_external_attribution(attribution))
        .map(|attribution| attribution.module_id)
        .collect::<BTreeSet<_>>();
    if report_external_modules.is_empty() {
        return ExternalImportSafetyReport::default();
    }
    let mut accepted_external_modules = rows
        .package_attributions
        .iter()
        .filter(|attribution| is_accepted_external_attribution(attribution))
        .map(|attribution| attribution.module_id)
        .chain(report_external_modules.iter().copied())
        .collect::<BTreeSet<_>>();
    let mut rejected = BTreeSet::<ModuleId>::new();

    loop {
        let mut changed = false;
        let source_suppressed_closure = external_import_source_suppressed_package_closure(
            &accepted_external_modules,
            &ownership_proven_modules,
            &modules_by_id,
            &outgoing_dependencies,
            &incoming_dependencies,
        );
        let source_boundary_modules = external_import_source_boundary_modules(
            &accepted_external_modules,
            &source_suppressed_closure,
            &modules_by_id,
            &incoming_dependencies,
        );
        for module_id in &report_external_modules {
            if rejected.contains(module_id) {
                continue;
            }
            if external_attribution_has_unexternalized_consumer(
                *module_id,
                &accepted_external_modules,
                &source_suppressed_closure,
                &source_boundary_modules,
                &modules_by_id,
                &incoming_dependencies,
            ) {
                rejected.insert(*module_id);
                accepted_external_modules.remove(module_id);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    if rejected.is_empty() {
        return ExternalImportSafetyReport::default();
    }
    let source_suppressed_closure = external_import_source_suppressed_package_closure(
        &accepted_external_modules,
        &ownership_proven_modules,
        &modules_by_id,
        &outgoing_dependencies,
        &incoming_dependencies,
    );
    let blockers = external_import_blocker_summaries(
        &rejected,
        &accepted_external_modules,
        &source_suppressed_closure,
        &external_import_source_boundary_modules(
            &accepted_external_modules,
            &source_suppressed_closure,
            &modules_by_id,
            &incoming_dependencies,
        ),
        &ownership_proven_modules,
        &modules_by_id,
        &incoming_dependencies,
    );
    let before = report.attributions.len();
    report
        .attributions
        .retain(|attribution| !rejected.contains(&attribution.module_id));
    ExternalImportSafetyReport {
        removed_modules: before.saturating_sub(report.attributions.len()),
        blockers,
    }
}

#[cfg(test)]
pub(crate) fn source_eliminated_package_modules_for_report(
    rows: &InputRows,
    report: &VersionedPackageMatchReport,
) -> usize {
    let loaded_package_modules = rows
        .modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
        .count();
    package_source_elimination_stats_for_report(rows, report, loaded_package_modules)
        .source_eliminated_package_modules
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct PackageSourceEliminationStats {
    pub(crate) direct_external_import_modules: usize,
    pub(crate) private_source_suppressed_package_modules: usize,
    pub(crate) source_eliminated_package_modules: usize,
    pub(crate) remaining_package_source_modules: usize,
}

pub(crate) fn package_source_elimination_stats_for_report(
    rows: &InputRows,
    report: &VersionedPackageMatchReport,
    loaded_package_modules: usize,
) -> PackageSourceEliminationStats {
    let accepted_external_modules = rows
        .package_attributions
        .iter()
        .chain(report.attributions.iter())
        .filter(|attribution| is_accepted_external_attribution(attribution))
        .map(|attribution| attribution.module_id)
        .collect::<BTreeSet<_>>();
    if accepted_external_modules.is_empty() {
        return PackageSourceEliminationStats {
            remaining_package_source_modules: loaded_package_modules,
            ..PackageSourceEliminationStats::default()
        };
    }
    let modules_by_id = rows
        .modules
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let (outgoing_dependencies, incoming_dependencies) = dependency_indexes(rows);
    let ownership_proven_modules = package_ownership_proven_modules(rows, report, &modules_by_id);
    let source_eliminated_modules = external_import_source_suppressed_package_closure(
        &accepted_external_modules,
        &ownership_proven_modules,
        &modules_by_id,
        &outgoing_dependencies,
        &incoming_dependencies,
    );
    let direct_external_import_modules = accepted_external_modules
        .iter()
        .filter(|module_id| {
            modules_by_id
                .get(module_id)
                .is_some_and(|module| module.kind == ModuleKind::Package)
        })
        .count();
    let source_eliminated_package_modules = source_eliminated_modules.len();
    PackageSourceEliminationStats {
        direct_external_import_modules,
        private_source_suppressed_package_modules: source_eliminated_package_modules
            .saturating_sub(direct_external_import_modules),
        source_eliminated_package_modules,
        remaining_package_source_modules: loaded_package_modules
            .saturating_sub(source_eliminated_package_modules),
    }
}

fn package_ownership_proven_modules(
    rows: &InputRows,
    report: &VersionedPackageMatchReport,
    modules_by_id: &BTreeMap<ModuleId, &ModuleInput>,
) -> BTreeSet<ModuleId> {
    let mut proven = BTreeSet::new();
    for attribution in &rows.package_attributions {
        if let Some(module) = modules_by_id.get(&attribution.module_id).copied()
            && package_attribution_proves_module_ownership(attribution, module)
        {
            proven.insert(attribution.module_id);
        }
    }
    for package_match in &report.matches {
        if let Some(module) = modules_by_id.get(&package_match.module_id).copied()
            && package_match_proves_module_ownership(package_match, module)
        {
            proven.insert(package_match.module_id);
        }
    }
    proven
}

fn package_attribution_proves_module_ownership(
    attribution: &PackageAttributionInput,
    module: &ModuleInput,
) -> bool {
    if module.kind != ModuleKind::Package
        || module.package_name.as_deref() != Some(attribution.package_name.as_str())
    {
        return false;
    }
    if let Some(attribution_version) = attribution.package_version.as_deref()
        && module
            .package_version
            .as_deref()
            .is_some_and(|module_version| {
                !module_version.trim().is_empty() && module_version != attribution_version
            })
    {
        return false;
    }
    is_accepted_external_attribution(attribution)
        || (attribution.status == PackageAttributionStatus::Rejected
            && attribution.emission_mode == PackageEmissionMode::ApplicationSource
            && attribution.package_version.is_some())
}

fn package_match_proves_module_ownership(
    package_match: &PackageMatch,
    module: &ModuleInput,
) -> bool {
    module.kind == ModuleKind::Package
        && module.package_name.as_deref() == Some(package_match.package_name.as_str())
        && module
            .package_version
            .as_deref()
            .is_none_or(|module_version| {
                module_version.trim().is_empty() || module_version == package_match.package_version
            })
}

fn external_import_blocker_summaries(
    rejected: &BTreeSet<ModuleId>,
    accepted_external_modules: &BTreeSet<ModuleId>,
    source_suppressed_closure: &BTreeSet<ModuleId>,
    source_boundary_modules: &BTreeSet<ModuleId>,
    ownership_proven_modules: &BTreeSet<ModuleId>,
    modules_by_id: &BTreeMap<ModuleId, &ModuleInput>,
    incoming_dependencies: &BTreeMap<ModuleId, Vec<ModuleId>>,
) -> Vec<ExternalImportBlockerSummary> {
    let mut counts = BTreeMap::<(String, String), usize>::new();
    for module_id in rejected {
        let Some(module) = modules_by_id.get(module_id).copied() else {
            continue;
        };
        for consumer_id in incoming_dependencies.get(module_id).into_iter().flatten() {
            if accepted_external_modules.contains(consumer_id)
                || source_suppressed_closure.contains(consumer_id)
                || source_boundary_modules.contains(consumer_id)
            {
                continue;
            }
            let Some(consumer) = modules_by_id.get(consumer_id).copied() else {
                continue;
            };
            if consumer_is_boundary(ConsumerBoundaryPolicy::ExternalImport, module, consumer) {
                continue;
            }
            let reason = match consumer.kind {
                ModuleKind::Application => continue,
                ModuleKind::Package if !ownership_proven_modules.contains(consumer_id) => {
                    "package consumer ownership not proven"
                }
                ModuleKind::Package => "package consumer not externalized",
                ModuleKind::Builtin => "builtin consumer not externalized",
            };
            let label = module_consumer_label(consumer);
            *counts.entry((reason.to_string(), label)).or_default() += 1;
        }
    }
    let mut blockers = counts
        .into_iter()
        .map(|((reason, consumer), count)| ExternalImportBlockerSummary {
            reason,
            consumer,
            count,
        })
        .collect::<Vec<_>>();
    blockers.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.reason.cmp(&right.reason))
            .then_with(|| left.consumer.cmp(&right.consumer))
    });
    blockers
}

fn module_consumer_label(module: &ModuleInput) -> String {
    match module.kind {
        ModuleKind::Package => {
            let package = module
                .package_name
                .as_deref()
                .unwrap_or("<unknown-package>");
            let version = module
                .package_version
                .as_deref()
                .unwrap_or("<unknown-version>");
            format!(
                "{package}@{version} module={} path={}",
                module.id.0, module.semantic_path
            )
        }
        ModuleKind::Application => {
            format!(
                "application module={} path={}",
                module.id.0, module.semantic_path
            )
        }
        ModuleKind::Builtin => {
            format!(
                "builtin module={} path={}",
                module.id.0, module.semantic_path
            )
        }
    }
}

fn external_attribution_has_unexternalized_consumer(
    module_id: ModuleId,
    accepted_external_modules: &BTreeSet<ModuleId>,
    source_suppressed_closure: &BTreeSet<ModuleId>,
    source_boundary_modules: &BTreeSet<ModuleId>,
    modules_by_id: &BTreeMap<ModuleId, &ModuleInput>,
    incoming_dependencies: &BTreeMap<ModuleId, Vec<ModuleId>>,
) -> bool {
    let Some(module) = modules_by_id.get(&module_id).copied() else {
        return false;
    };
    for consumer_id in incoming_dependencies.get(&module_id).into_iter().flatten() {
        if accepted_external_modules.contains(consumer_id)
            || source_suppressed_closure.contains(consumer_id)
            || source_boundary_modules.contains(consumer_id)
        {
            continue;
        }
        if modules_by_id.get(consumer_id).is_some_and(|consumer| {
            !consumer_is_boundary(ConsumerBoundaryPolicy::ExternalImport, module, consumer)
        }) {
            return true;
        }
    }
    false
}

fn external_import_source_boundary_modules(
    accepted_external_modules: &BTreeSet<ModuleId>,
    source_suppressed_closure: &BTreeSet<ModuleId>,
    modules_by_id: &BTreeMap<ModuleId, &ModuleInput>,
    incoming_dependencies: &BTreeMap<ModuleId, Vec<ModuleId>>,
) -> BTreeSet<ModuleId> {
    let mut boundary = modules_by_id
        .iter()
        .filter_map(|(module_id, module)| {
            (module.kind == ModuleKind::Package
                && !accepted_external_modules.contains(module_id)
                && !source_suppressed_closure.contains(module_id))
            .then_some(*module_id)
        })
        .collect::<BTreeSet<_>>();
    loop {
        let mut removed = Vec::new();
        for module_id in &boundary {
            let Some(module) = modules_by_id.get(module_id).copied() else {
                continue;
            };
            let has_unresolved_same_package_consumer = incoming_dependencies
                .get(module_id)
                .into_iter()
                .flatten()
                .any(|consumer_id| {
                    modules_by_id.get(consumer_id).is_some_and(|consumer| {
                        consumer.kind == ModuleKind::Package
                            && same_package_consumer(module, consumer)
                            && !accepted_external_modules.contains(consumer_id)
                            && !source_suppressed_closure.contains(consumer_id)
                            && !boundary.contains(consumer_id)
                    })
                });
            if has_unresolved_same_package_consumer {
                removed.push(*module_id);
            }
        }
        if removed.is_empty() {
            break;
        }
        for module_id in removed {
            boundary.remove(&module_id);
        }
    }
    boundary
}

fn external_import_source_suppressed_package_closure(
    accepted_external_modules: &BTreeSet<ModuleId>,
    ownership_proven_modules: &BTreeSet<ModuleId>,
    modules_by_id: &BTreeMap<ModuleId, &ModuleInput>,
    outgoing_dependencies: &BTreeMap<ModuleId, Vec<ModuleId>>,
    incoming_dependencies: &BTreeMap<ModuleId, Vec<ModuleId>>,
) -> BTreeSet<ModuleId> {
    let mut reachable = accepted_external_modules
        .iter()
        .copied()
        .filter(|module_id| {
            modules_by_id
                .get(module_id)
                .is_some_and(|module| module.kind == ModuleKind::Package)
        })
        .collect::<BTreeSet<_>>();
    let mut stack = reachable.iter().copied().collect::<Vec<_>>();
    while let Some(module_id) = stack.pop() {
        for dependency_id in outgoing_dependencies
            .get(&module_id)
            .into_iter()
            .flatten()
            .copied()
        {
            let Some(dependency) = modules_by_id.get(&dependency_id) else {
                continue;
            };
            if dependency.kind != ModuleKind::Package
                || !ownership_proven_modules.contains(&dependency_id)
                || !reachable.insert(dependency_id)
            {
                continue;
            }
            stack.push(dependency_id);
        }
    }

    let seed_modules = accepted_external_modules.clone();
    loop {
        let mut removed = Vec::new();
        for module_id in &reachable {
            if seed_modules.contains(module_id) {
                continue;
            }
            let Some(module) = modules_by_id.get(module_id).copied() else {
                continue;
            };
            let has_external_consumer = incoming_dependencies
                .get(module_id)
                .into_iter()
                .flatten()
                .any(|consumer_id| {
                    modules_by_id.get(consumer_id).is_some_and(|consumer| {
                        !reachable.contains(consumer_id)
                            && !consumer_is_boundary(
                                ConsumerBoundaryPolicy::SourceSuppressed,
                                module,
                                consumer,
                            )
                    })
                });
            if has_external_consumer {
                removed.push(*module_id);
            }
        }
        if removed.is_empty() {
            break;
        }
        for module_id in removed {
            reachable.remove(&module_id);
        }
    }
    reachable
}

pub(crate) fn externalization_chain_proofs(
    rows: &InputRows,
    report: &VersionedPackageMatchReport,
) -> BTreeMap<ModuleId, serde_json::Value> {
    let modules_by_id = rows
        .modules
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let (outgoing_dependencies, incoming_dependencies) = dependency_indexes(rows);
    let accepted_external_modules = rows
        .package_attributions
        .iter()
        .chain(report.attributions.iter())
        .filter(|attribution| is_accepted_external_attribution(attribution))
        .map(|attribution| attribution.module_id)
        .collect::<BTreeSet<_>>();
    let ownership_proven_modules = package_ownership_proven_modules(rows, report, &modules_by_id);
    let source_suppressed_closure = external_import_source_suppressed_package_closure(
        &accepted_external_modules,
        &ownership_proven_modules,
        &modules_by_id,
        &outgoing_dependencies,
        &incoming_dependencies,
    );
    let source_boundary_modules = external_import_source_boundary_modules(
        &accepted_external_modules,
        &source_suppressed_closure,
        &modules_by_id,
        &incoming_dependencies,
    );
    report
        .attributions
        .iter()
        .filter(|attribution| is_accepted_external_attribution(attribution))
        .filter_map(|attribution| {
            let module = modules_by_id.get(&attribution.module_id).copied()?;
            let suppressed_dependencies = source_suppressed_dependencies_for_seed(
                attribution.module_id,
                &accepted_external_modules,
                &source_suppressed_closure,
                &outgoing_dependencies,
            );
            let incoming_consumers = incoming_dependencies
                .get(&attribution.module_id)
                .into_iter()
                .flatten()
                .filter_map(|consumer_id| {
                    let consumer = modules_by_id.get(consumer_id).copied()?;
                    let resolution = if accepted_external_modules.contains(consumer_id) {
                        "direct_externalized"
                    } else if source_suppressed_closure.contains(consumer_id) {
                        "source_suppressed"
                    } else if consumer.kind == ModuleKind::Application {
                        "application_boundary"
                    } else if consumer.kind == ModuleKind::Builtin {
                        "builtin_boundary"
                    } else if consumer_is_boundary(
                        ConsumerBoundaryPolicy::ExternalImport,
                        module,
                        consumer,
                    ) {
                        "package_boundary"
                    } else if source_boundary_modules.contains(consumer_id) {
                        "source_boundary"
                    } else {
                        "unresolved"
                    };
                    Some(serde_json::json!({
                        "module_id": consumer_id.0,
                        "kind": module_kind_label(consumer.kind),
                        "package_name": consumer.package_name.as_deref(),
                        "package_version": consumer.package_version.as_deref(),
                        "semantic_path": consumer.semantic_path.as_str(),
                        "resolution": resolution,
                    }))
                })
                .take(64)
                .collect::<Vec<_>>();
            Some((
                attribution.module_id,
                serde_json::json!({
                    "proof_model": "externalization_chain_v1",
                    "direct_seed_module_id": attribution.module_id.0,
                    "direct_seed_kind": module_kind_label(module.kind),
                    "ownership_proof": "direct_external_import",
                    "all_incoming_consumers_resolved": incoming_consumers
                        .iter()
                        .all(|consumer| consumer
                            .get("resolution")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|resolution| resolution != "unresolved")),
                    "incoming_consumers": incoming_consumers,
                    "source_suppressed_dependency_count": suppressed_dependencies.len(),
                    "source_suppressed_dependency_module_ids": suppressed_dependencies
                        .iter()
                        .take(64)
                        .map(|module_id| module_id.0)
                        .collect::<Vec<_>>(),
                }),
            ))
        })
        .collect()
}

fn source_suppressed_dependencies_for_seed(
    seed: ModuleId,
    accepted_external_modules: &BTreeSet<ModuleId>,
    source_suppressed_closure: &BTreeSet<ModuleId>,
    outgoing_dependencies: &BTreeMap<ModuleId, Vec<ModuleId>>,
) -> BTreeSet<ModuleId> {
    let mut dependencies = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut stack = outgoing_dependencies
        .get(&seed)
        .into_iter()
        .flatten()
        .copied()
        .collect::<Vec<_>>();
    while let Some(module_id) = stack.pop() {
        if !visited.insert(module_id) || !source_suppressed_closure.contains(&module_id) {
            continue;
        }
        if !accepted_external_modules.contains(&module_id) {
            dependencies.insert(module_id);
        }
        stack.extend(
            outgoing_dependencies
                .get(&module_id)
                .into_iter()
                .flatten()
                .copied(),
        );
    }
    dependencies
}

const fn module_kind_label(kind: ModuleKind) -> &'static str {
    match kind {
        ModuleKind::Application => "application",
        ModuleKind::Package => "package",
        ModuleKind::Builtin => "builtin",
    }
}

fn package_source_quality_rejection_reason(
    rows: &InputRows,
    module: &ModuleInput,
    package_name: &str,
    modules_by_id: &BTreeMap<ModuleId, &ModuleInput>,
    outgoing_dependencies: &BTreeMap<ModuleId, Vec<ModuleId>>,
) -> Option<&'static str> {
    let Some(slice) = rows.module_source_slice(module.id) else {
        return Some(
            "package module has no source slice, so package ownership could not be verified",
        );
    };
    let quality = package_module_source_quality(module, slice.source_file_path, slice.source);
    if quality == PackageModuleSourceQuality::Invalid {
        return Some(
            "package module source slice is not parseable, so package ownership could not be verified",
        );
    }
    if quality != PackageModuleSourceQuality::Weak {
        return None;
    }
    let mut same_package_dependencies = 0usize;
    let mut other_package_dependencies = 0usize;
    for dependency_id in outgoing_dependencies.get(&module.id).into_iter().flatten() {
        let Some(dependency) = modules_by_id.get(dependency_id) else {
            continue;
        };
        let Some(dependency_package_name) = dependency.package_name.as_deref() else {
            continue;
        };
        if dependency_package_name == package_name {
            same_package_dependencies += 1;
        } else {
            other_package_dependencies += 1;
        }
    }
    if other_package_dependencies > 0 && same_package_dependencies == 0 {
        Some(
            "package hint is weak and direct dependency graph points at other packages; no safe package ownership match was accepted",
        )
    } else {
        Some(
            "package hint is weak because the module source does not contain strong package path tokens; no package ownership evidence matched",
        )
    }
}

fn decision_package_name(decision: &BestVersionMatch) -> &str {
    match decision {
        BestVersionMatch::Selected { score, .. }
        | BestVersionMatch::InsufficientEvidence { score } => score.package_name.as_str(),
        BestVersionMatch::Ambiguous { package_name, .. }
        | BestVersionMatch::NoMatch { package_name, .. } => package_name.as_str(),
    }
}

fn rejection_reason_from_decision(decision: &BestVersionMatch) -> String {
    match decision {
        BestVersionMatch::Selected { module_matches, .. }
            if module_matches
                .iter()
                .all(|module_match| !module_match.external_importable) =>
        {
            "selected package source is source-only and has not been proven external-importable"
                .to_string()
        }
        BestVersionMatch::Selected { .. } => {
            "selected package version did not match this module source".to_string()
        }
        BestVersionMatch::Ambiguous { .. } => {
            "package version matching found more than one best version".to_string()
        }
        BestVersionMatch::NoMatch { scores, .. } if scores.is_empty() => {
            "no cached package source was available for this package".to_string()
        }
        BestVersionMatch::NoMatch { .. } => {
            "package version matching found no usable evidence".to_string()
        }
        BestVersionMatch::InsufficientEvidence { .. } => {
            "package version evidence did not satisfy the acceptance threshold".to_string()
        }
    }
}

struct PackageDiagnosticsContext<'a> {
    modules_by_id: BTreeMap<ModuleId, &'a ModuleInput>,
    ownership_by_module: BTreeMap<ModuleId, (String, String)>,
    outgoing_dependencies: BTreeMap<ModuleId, Vec<ModuleId>>,
    incoming_dependencies: BTreeMap<ModuleId, Vec<ModuleId>>,
    version_decisions_by_package: BTreeMap<String, &'a BestVersionMatch>,
}

impl<'a> PackageDiagnosticsContext<'a> {
    fn new(rows: &'a InputRows, report: &'a VersionedPackageMatchReport) -> Self {
        let modules_by_id = rows
            .modules
            .iter()
            .map(|module| (module.id, module))
            .collect::<BTreeMap<_, _>>();
        let (outgoing_dependencies, incoming_dependencies) = dependency_indexes(rows);
        let version_decisions_by_package = report
            .version_matches
            .iter()
            .map(|decision| (decision_package_name(decision).to_string(), decision))
            .collect::<BTreeMap<_, _>>();
        Self {
            modules_by_id,
            ownership_by_module: ownership_by_module(rows, report),
            outgoing_dependencies,
            incoming_dependencies,
            version_decisions_by_package,
        }
    }
}

fn dependency_indexes(
    rows: &InputRows,
) -> (
    BTreeMap<ModuleId, Vec<ModuleId>>,
    BTreeMap<ModuleId, Vec<ModuleId>>,
) {
    let mut outgoing = BTreeMap::<ModuleId, Vec<ModuleId>>::new();
    let mut incoming = BTreeMap::<ModuleId, Vec<ModuleId>>::new();
    for dependency in &rows.dependencies {
        let ModuleDependencyTarget::Module(target) = dependency.target else {
            continue;
        };
        outgoing
            .entry(dependency.from_module_id)
            .or_default()
            .push(target);
        incoming
            .entry(target)
            .or_default()
            .push(dependency.from_module_id);
    }
    (outgoing, incoming)
}

fn unmatched_package_diagnostics(
    rows: &InputRows,
    context: &PackageDiagnosticsContext<'_>,
    module: &ModuleInput,
) -> serde_json::Value {
    let package_name = module.package_name.as_deref().unwrap_or_default();
    serde_json::json!({
        "module_id": module.id.0,
        "module_original_name": module.original_name,
        "semantic_path": module.semantic_path,
        "package_hint": {
            "package_name": module.package_name,
            "package_version": module.package_version,
        },
        "source_slice": source_slice_diagnostics(rows, module),
        "dependency_neighborhood": dependency_neighborhood_diagnostics(context, module.id),
        "version_decision": version_decision_diagnostics(context, package_name),
    })
}

fn source_slice_diagnostics(rows: &InputRows, module: &ModuleInput) -> serde_json::Value {
    let Some(slice) = rows.module_source_slice(module.id) else {
        return serde_json::json!({
            "available": false,
            "source_file_id": module.source_file_id,
            "has_source_span": module.source_span.is_some(),
            "reason": "missing_or_ambiguous_source_slice",
        });
    };
    let quality = package_module_source_quality(module, slice.source_file_path, slice.source);
    serde_json::json!({
        "available": true,
        "source_file_id": slice.source_file_id,
        "source_file_path": slice.source_file_path,
        "has_source_span": slice.span.is_some(),
        "byte_start": slice.span.map(|span| span.byte_start),
        "byte_end": slice.span.map(|span| span.byte_end),
        "source_len": slice.source.len(),
        "quality": package_source_quality_label(quality),
        "function_count": FunctionExtractor::function_count(module.id, slice.source),
    })
}

fn package_source_quality_label(quality: PackageModuleSourceQuality) -> &'static str {
    match quality {
        PackageModuleSourceQuality::Trusted => "trusted",
        PackageModuleSourceQuality::Weak => "weak",
        PackageModuleSourceQuality::Invalid => "invalid",
    }
}

fn dependency_neighborhood_diagnostics(
    context: &PackageDiagnosticsContext<'_>,
    module_id: ModuleId,
) -> serde_json::Value {
    let outgoing_ids = context
        .outgoing_dependencies
        .get(&module_id)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let incoming_ids = context
        .incoming_dependencies
        .get(&module_id)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let outgoing = dependency_package_summary(
        outgoing_ids,
        &context.modules_by_id,
        &context.ownership_by_module,
    );
    let incoming = dependency_package_summary(
        incoming_ids,
        &context.modules_by_id,
        &context.ownership_by_module,
    );
    serde_json::json!({
        "outgoing_package_counts": outgoing.package_counts.clone(),
        "incoming_package_counts": incoming.package_counts.clone(),
        "outgoing_owned_package_counts": outgoing.owned_package_counts.clone(),
        "incoming_owned_package_counts": incoming.owned_package_counts.clone(),
        "outgoing": dependency_package_summary_json(&outgoing),
        "incoming": dependency_package_summary_json(&incoming),
    })
}

#[derive(Debug, Clone)]
struct DependencyPackageSummary {
    module_count: usize,
    package_counts: BTreeMap<String, usize>,
    owned_module_count: usize,
    owned_package_counts: BTreeMap<String, usize>,
    owned_version_counts: BTreeMap<String, usize>,
}

fn dependency_package_summary(
    module_ids: &[ModuleId],
    modules_by_id: &BTreeMap<ModuleId, &ModuleInput>,
    ownership_by_module: &BTreeMap<ModuleId, (String, String)>,
) -> DependencyPackageSummary {
    let mut seen = BTreeSet::new();
    let mut package_counts = BTreeMap::<String, usize>::new();
    let mut owned_package_counts = BTreeMap::<String, usize>::new();
    let mut owned_version_counts = BTreeMap::<String, usize>::new();
    let mut module_count = 0usize;
    let mut owned_module_count = 0usize;
    for module_id in module_ids.iter().copied() {
        if !seen.insert(module_id) {
            continue;
        }
        let Some(module) = modules_by_id.get(&module_id) else {
            continue;
        };
        module_count += 1;
        if let Some(package_name) = module.package_name.as_deref() {
            *package_counts.entry(package_name.to_string()).or_default() += 1;
        }
        let Some((owned_package_name, owned_package_version)) = ownership_by_module.get(&module_id)
        else {
            continue;
        };
        owned_module_count += 1;
        *owned_package_counts
            .entry(owned_package_name.clone())
            .or_default() += 1;
        *owned_version_counts
            .entry(format!("{owned_package_name}@{owned_package_version}"))
            .or_default() += 1;
    }
    DependencyPackageSummary {
        module_count,
        package_counts,
        owned_module_count,
        owned_package_counts,
        owned_version_counts,
    }
}

fn dependency_package_summary_json(summary: &DependencyPackageSummary) -> serde_json::Value {
    serde_json::json!({
        "module_count": summary.module_count,
        "package_counts": summary.package_counts,
        "owned_module_count": summary.owned_module_count,
        "owned_package_counts": summary.owned_package_counts,
        "owned_version_counts": summary.owned_version_counts,
    })
}

fn version_decision_diagnostics(
    context: &PackageDiagnosticsContext<'_>,
    package_name: &str,
) -> serde_json::Value {
    let Some(decision) = context.version_decisions_by_package.get(package_name) else {
        return serde_json::json!({
            "kind": "not_evaluated",
            "top_scores": [],
        });
    };
    let mut scores = decision_scores(decision);
    scores.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| right.package_version.cmp(&left.package_version))
    });
    let top_scores = scores
        .into_iter()
        .take(3)
        .map(version_score_json)
        .collect::<Vec<_>>();
    serde_json::json!({
        "kind": decision_kind(decision),
        "reason": rejection_reason_from_decision(decision),
        "top_scores": top_scores,
    })
}

fn decision_kind(decision: &BestVersionMatch) -> &'static str {
    match decision {
        BestVersionMatch::Selected { .. } => "selected",
        BestVersionMatch::Ambiguous { .. } => "ambiguous",
        BestVersionMatch::NoMatch { .. } => "no_match",
        BestVersionMatch::InsufficientEvidence { .. } => "insufficient_evidence",
    }
}

fn decision_scores(decision: &BestVersionMatch) -> Vec<&VersionMatchScore> {
    match decision {
        BestVersionMatch::Selected { score, .. }
        | BestVersionMatch::InsufficientEvidence { score } => vec![score],
        BestVersionMatch::Ambiguous { scores, .. } | BestVersionMatch::NoMatch { scores, .. } => {
            scores.iter().collect()
        }
    }
}

fn version_score_json(score: &VersionMatchScore) -> serde_json::Value {
    serde_json::json!({
        "package_name": score.package_name,
        "package_version": score.package_version,
        "score": score.score,
        "total_modules": score.total_modules,
        "matched_modules": score.matched_modules,
        "source_hash_matches": score.source_hash_matches,
        "function_signature_matches": score.function_signature_matches,
        "string_anchor_matches": score.string_anchor_matches,
        "binary_search_probes": score.binary_search_probes,
    })
}

fn persist_package_attribution(
    connection: &Connection,
    module_original_name: &str,
    attribution: &PackageAttributionInput,
    module_match: &PackageMatch,
    version_resolution: Option<&PackageVersionResolutionEvidence>,
    externalization_chain: Option<&serde_json::Value>,
) -> Result<(), MatchPackagesError> {
    let package_version =
        attribution
            .package_version
            .as_deref()
            .ok_or(MatchPackagesError::InvalidAttribution {
                module_id: attribution.module_id,
                message: "accepted package attribution has no package version".to_string(),
            })?;
    let export_specifier =
        attribution
            .export_specifier
            .as_deref()
            .ok_or(MatchPackagesError::InvalidAttribution {
                module_id: attribution.module_id,
                message: "accepted external package attribution has no export specifier"
                    .to_string(),
            })?;
    let version_resolution = version_resolution.map(|resolution| {
        serde_json::json!({
            "requested_version": resolution.requested_version,
            "resolved_version": resolution.resolved_version,
            "reason": resolution.reason,
        })
    });
    let evidence = serde_json::json!({
        "matcher": "exact_normalized_source_binary_search",
        "package_name": module_match.package_name,
        "package_version": module_match.package_version,
        "export_specifier": module_match.export_specifier,
        "source_path": module_match.source_path,
        "normalized_source_hash": module_match.normalized_source_hash,
        "match_strategy": module_match.strategy.as_str(),
        "external_import_proof": external_import_proof_label(module_match.source_path.as_str()),
        "version_resolution": version_resolution,
        "function_signature_matches": module_match.function_signature_matches,
        "string_anchor_matches": module_match.string_anchor_matches,
        "writes_package_version": true,
        "external_import_policy_version": PACKAGE_ATTRIBUTION_EXTERNAL_IMPORT_POLICY_VERSION,
        "externalization_chain": externalization_chain,
    })
    .to_string();
    connection
        .execute(
            r"
            INSERT INTO package_attributions
                (module_id, module_original_name, package_name, package_version,
                 package_subpath, resolved_file, export_specifier, emission_mode,
                 status, evidence_json, rejection_reason, external_import_policy_version,
                 created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'external_import',
                    'accepted', ?8, NULL, ?9, datetime('now'), datetime('now'))
            ON CONFLICT(module_id) DO UPDATE SET
                module_original_name = excluded.module_original_name,
                package_name = excluded.package_name,
                package_version = excluded.package_version,
                package_subpath = excluded.package_subpath,
                resolved_file = excluded.resolved_file,
                export_specifier = excluded.export_specifier,
                emission_mode = excluded.emission_mode,
                status = excluded.status,
                evidence_json = excluded.evidence_json,
                rejection_reason = excluded.rejection_reason,
                external_import_policy_version = excluded.external_import_policy_version,
                updated_at = datetime('now')
            ",
            params![
                i64::from(attribution.module_id.0),
                module_original_name,
                attribution.package_name.as_str(),
                package_version,
                attribution.subpath.as_deref(),
                module_match.source_path.as_str(),
                export_specifier,
                evidence,
                PACKAGE_ATTRIBUTION_EXTERNAL_IMPORT_POLICY_VERSION,
            ],
        )
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(())
}

fn persist_rejected_package_attribution(
    connection: &Connection,
    module_original_name: &str,
    attribution: &PackageAttributionInput,
    module_match: Option<&PackageMatch>,
    unmatched_diagnostics: serde_json::Value,
) -> Result<(), MatchPackagesError> {
    let rejection_reason =
        attribution
            .rejection_reason
            .as_deref()
            .ok_or(MatchPackagesError::InvalidAttribution {
                module_id: attribution.module_id,
                message: "rejected package attribution has no rejection reason".to_string(),
            })?;
    if rejection_reason.trim().is_empty() {
        return Err(MatchPackagesError::InvalidAttribution {
            module_id: attribution.module_id,
            message: "rejected package attribution has empty rejection reason".to_string(),
        });
    }

    let match_evidence = module_match.map(|module_match| {
        serde_json::json!({
            "package_name": module_match.package_name,
            "package_version": module_match.package_version,
            "export_specifier": module_match.export_specifier,
            "source_path": module_match.source_path,
            "normalized_source_hash": module_match.normalized_source_hash,
            "match_strategy": module_match.strategy.as_str(),
            "function_signature_matches": module_match.function_signature_matches,
            "string_anchor_matches": module_match.string_anchor_matches,
            "external_importable": module_match.external_importable,
        })
    });
    let evidence = serde_json::json!({
        "matcher": "package_ownership_matcher",
        "package_name": attribution.package_name,
        "package_version": attribution.package_version,
        "status": "rejected",
        "rejection_reason": rejection_reason,
        "ownership_match": match_evidence,
        "unmatched_diagnostics": unmatched_diagnostics,
        "writes_external_import": false,
    })
    .to_string();
    let resolved_file = module_match.map(|module_match| module_match.source_path.as_str());
    connection
        .execute(
            r"
            INSERT INTO package_attributions
                (module_id, module_original_name, package_name, package_version,
                 package_subpath, resolved_file, export_specifier, emission_mode,
                 status, evidence_json, rejection_reason, external_import_policy_version,
                 created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'application_source',
                    'rejected', ?8, ?9, 0, datetime('now'), datetime('now'))
            ON CONFLICT(module_id) DO UPDATE SET
                module_original_name = excluded.module_original_name,
                package_name = excluded.package_name,
                package_version = excluded.package_version,
                package_subpath = excluded.package_subpath,
                resolved_file = excluded.resolved_file,
                export_specifier = excluded.export_specifier,
                emission_mode = excluded.emission_mode,
                status = excluded.status,
                evidence_json = excluded.evidence_json,
                rejection_reason = excluded.rejection_reason,
                external_import_policy_version = excluded.external_import_policy_version,
                updated_at = datetime('now')
            ",
            params![
                i64::from(attribution.module_id.0),
                module_original_name,
                attribution.package_name.as_str(),
                attribution.package_version.as_deref(),
                attribution.subpath.as_deref(),
                resolved_file,
                attribution.export_specifier.as_deref(),
                evidence,
                rejection_reason,
            ],
        )
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(())
}
