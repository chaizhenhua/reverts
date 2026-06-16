//! `package-cache-audit`, `package-cache-prune-stale`, and
//! `package-externalization-hints` command runners.
//!
//! These three commands share the package-cache subsystem and produce
//! similarly shaped audit outputs, so they live together as one
//! presentation module.

use crate::args::{PackageCacheArgs, PackageExternalizationHintsArgs};
use crate::errors::CliRunError;
use crate::{
    PackageCacheAuditOutcome, package_cache_audit_from_sqlite,
    package_externalization_hints_from_sqlite,
};

pub(crate) fn run_audit(args: PackageCacheArgs) -> Result<(), CliRunError> {
    let outcome =
        package_cache_audit_from_sqlite(&args, false).map_err(CliRunError::MatchPackages)?;
    print_package_cache_audit(&outcome, false);
    Ok(())
}

pub(crate) fn run_prune_stale(args: PackageCacheArgs) -> Result<(), CliRunError> {
    let outcome =
        package_cache_audit_from_sqlite(&args, true).map_err(CliRunError::MatchPackages)?;
    print_package_cache_audit(&outcome, args.apply);
    Ok(())
}

pub(crate) fn run_externalization_hints(
    args: PackageExternalizationHintsArgs,
) -> Result<(), CliRunError> {
    let outcome =
        package_externalization_hints_from_sqlite(&args).map_err(CliRunError::MatchPackages)?;
    println!(
        "package externalization hints: scanned={}, verified={}, skipped_invalid_specifier={}, skipped_invalid_versions={}, skipped_hash_mismatch={}, skipped_normalize_errors={}, {}={}",
        outcome.scanned_rows,
        outcome.verified_rows,
        outcome.invalid_export_specifier_rows,
        outcome.invalid_version_rows,
        outcome.content_hash_mismatch_rows,
        outcome.normalize_error_rows,
        if args.apply { "written" } else { "would_write" },
        outcome.written_rows,
    );
    Ok(())
}

fn print_package_cache_audit(outcome: &PackageCacheAuditOutcome, applied: bool) {
    println!(
        "package source cache audit: rows={}, missing_identity={}, invalid_versions={}, stale_policy={}, missing_policy={}, missing_export_specifier={}, parse_errors={}, {}={}",
        outcome.rows,
        outcome.missing_identity_rows,
        outcome.invalid_version_rows,
        outcome.stale_policy_rows,
        outcome.missing_import_policy_rows,
        outcome.missing_export_specifier_rows,
        outcome.parse_error_rows,
        if applied { "deleted" } else { "would_delete" },
        outcome.deleted_rows,
    );
}
