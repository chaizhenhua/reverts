//! `match-packages` and `match-packages-report` command runners.
//!
//! Both runners drive the matcher pipeline via `match_packages_*_from_sqlite`
//! (still in `lib.rs`), then print human-readable summaries. They share the
//! `print_external_import_blockers` helper.

use crate::args::{MatchPackagesArgs, MatchPackagesReportArgs};
use crate::errors::CliRunError;
use crate::{
    ExternalImportBlockerSummary, format_audit_findings, match_packages_from_sqlite,
    match_packages_report_from_sqlite, pct,
};

pub(crate) fn run(args: MatchPackagesArgs) -> Result<(), CliRunError> {
    let outcome = match_packages_from_sqlite(&args).map_err(CliRunError::MatchPackages)?;
    println!(
        "matched packages for project {} from {} package source(s): {} module attribution(s), {} direct external import module attribution(s), {} private source-suppressed package module(s), {} package source eliminated ({:.2}%), {} package source remaining, {} external import candidate(s), {} unsafe external import candidate(s) removed, {} package surface(s), {} attribution(s) written, {} surface(s) written, {} function attribution(s) ({} written), {} function ownership match(es), {} trusted / {} weak / {} invalid / {} missing package module source slice(s), {} audit finding(s)",
        outcome.project_id,
        outcome.loaded_package_sources,
        outcome.matched_modules,
        outcome.external_import_modules,
        outcome.private_source_suppressed_package_modules,
        outcome.source_eliminated_package_modules,
        pct(
            outcome.source_eliminated_package_modules,
            outcome.loaded_package_modules
        ),
        outcome.remaining_package_source_modules,
        outcome.external_import_candidates,
        outcome.unsafe_external_import_modules,
        outcome.matched_package_surfaces,
        outcome.written_attributions,
        outcome.written_surfaces,
        outcome.function_attributions,
        outcome.written_function_attributions,
        outcome.function_ownership_matches,
        outcome.package_source_quality_trusted,
        outcome.package_source_quality_weak,
        outcome.package_source_quality_invalid,
        outcome.package_source_quality_missing,
        outcome.audit.findings().len()
    );
    if !outcome.audit.is_clean() {
        println!("{}", format_audit_findings(&outcome.audit));
    }
    print_external_import_blockers(&outcome.external_import_blockers);
    Ok(())
}

pub(crate) fn run_report(args: MatchPackagesReportArgs) -> Result<(), CliRunError> {
    let outcome = match_packages_report_from_sqlite(&args).map_err(CliRunError::MatchPackages)?;
    println!(
        "package match report: projects={}, package_modules={}, matched={} ({:.2}%), direct_externalized={} ({:.2}% of package modules), private_source_suppressed={}, source_eliminated={} ({:.2}% of package modules), source_remaining={}, candidates={}, unsafe_removed={}, surfaces={}, audit_findings={}",
        outcome.projects.len(),
        outcome.totals.package_modules,
        outcome.totals.matched_modules,
        pct(
            outcome.totals.matched_modules,
            outcome.totals.package_modules
        ),
        outcome.totals.external_import_modules,
        pct(
            outcome.totals.external_import_modules,
            outcome.totals.package_modules
        ),
        outcome.totals.private_source_suppressed_package_modules,
        outcome.totals.source_eliminated_package_modules,
        pct(
            outcome.totals.source_eliminated_package_modules,
            outcome.totals.package_modules
        ),
        outcome.totals.remaining_package_source_modules,
        outcome.totals.external_import_candidates,
        outcome.totals.unsafe_external_import_modules,
        outcome.totals.package_surfaces,
        outcome.totals.audit_findings,
    );
    for project in &outcome.projects {
        println!(
            "  project {}: package_modules={}, matched={} ({:.2}%), direct_externalized={} ({:.2}% of package modules), private_source_suppressed={}, source_eliminated={} ({:.2}% of package modules), source_remaining={}, candidates={}, unsafe_removed={}, surfaces={}, audit_findings={}",
            project.project_id,
            project.loaded_package_modules,
            project.matched_modules,
            pct(project.matched_modules, project.loaded_package_modules),
            project.external_import_modules,
            pct(
                project.external_import_modules,
                project.loaded_package_modules
            ),
            project.private_source_suppressed_package_modules,
            project.source_eliminated_package_modules,
            pct(
                project.source_eliminated_package_modules,
                project.loaded_package_modules
            ),
            project.remaining_package_source_modules,
            project.external_import_candidates,
            project.unsafe_external_import_modules,
            project.matched_package_surfaces,
            project.audit.findings().len(),
        );
    }
    print_external_import_blockers(&outcome.blockers);
    Ok(())
}

fn print_external_import_blockers(blockers: &[ExternalImportBlockerSummary]) {
    if blockers.is_empty() {
        return;
    }
    println!("external import blockers:");
    for blocker in blockers.iter().take(12) {
        println!(
            "  {}: {} ({})",
            blocker.reason, blocker.consumer, blocker.count
        );
    }
}
