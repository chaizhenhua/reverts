//! `package-cache-audit`, `package-cache-prune-stale`, and
//! `package-externalization-hints` command runners + the audit/prune
//! analysis pipeline.
//!
//! The audit functions ([`package_cache_audit_from_sqlite`] +
//! [`package_cache_audit_from_connection`]) read the
//! `package_source_cache` table, count missing-policy / invalid-version
//! / parse-error / etc. rows, and (when `prune_stale` is set) delete
//! the same rows via [`prune_package_source_cache_rows`]. The runner
//! glue and the human-readable single-line summary live here too. The
//! externalization-hints subcommand runner stays here to share the
//! `package-cache` presentation module; its analysis still lives in
//! `lib.rs` for now.

use std::path::Path;

use reverts_js::normalize_source_for_pipeline;
use reverts_package_matcher::is_exact_package_version_hint;
use rusqlite::{Connection, OpenFlags};
use std::time::Duration;

use crate::args::{PackageCacheArgs, PackageExternalizationHintsArgs};
use crate::errors::{CliRunError, MatchPackagesError};
use crate::{
    PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION, collect_sqlite_rows,
    package_externalization_hints_from_sqlite, sqlite_table_exists, sqlite_table_has_column,
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PackageCacheAuditOutcome {
    pub rows: usize,
    pub missing_identity_rows: usize,
    pub invalid_version_rows: usize,
    pub stale_policy_rows: usize,
    pub missing_export_specifier_rows: usize,
    pub missing_import_policy_rows: usize,
    pub parse_error_rows: usize,
    pub deleted_rows: usize,
}

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

pub fn package_cache_audit_from_sqlite(
    args: &PackageCacheArgs,
    prune_stale: bool,
) -> Result<PackageCacheAuditOutcome, MatchPackagesError> {
    let flags = if prune_stale && args.apply {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let mut connection =
        Connection::open_with_flags(args.input.as_path(), flags).map_err(|source| {
            MatchPackagesError::OpenDatabase {
                path: args.input.clone(),
                source,
            }
        })?;
    connection
        .busy_timeout(Duration::from_secs(30))
        .map_err(MatchPackagesError::ConfigureDatabase)?;
    if !sqlite_table_exists(&connection, "package_source_cache")
        .map_err(MatchPackagesError::QueryPackageSources)?
    {
        return Err(MatchPackagesError::MissingTable("package_source_cache"));
    }
    let audit = package_cache_audit_from_connection(&connection)?;
    if prune_stale && args.apply && audit.deleted_rows > 0 {
        prune_package_source_cache_rows(&mut connection)?;
    }
    Ok(audit)
}

fn package_cache_audit_from_connection(
    connection: &Connection,
) -> Result<PackageCacheAuditOutcome, MatchPackagesError> {
    let rows = audited_package_source_cache_rows(connection)?;
    let mut outcome = PackageCacheAuditOutcome {
        rows: rows.len(),
        ..PackageCacheAuditOutcome::default()
    };
    for row in rows {
        if row.missing_identity {
            outcome.missing_identity_rows += 1;
        }
        if !is_exact_package_version_hint(row.package_version.as_str()) {
            outcome.invalid_version_rows += 1;
        }
        if row.missing_policy {
            outcome.missing_import_policy_rows += 1;
        }
        if row.missing_export_specifier {
            outcome.missing_export_specifier_rows += 1;
        }
        if row.stale_policy {
            outcome.stale_policy_rows += 1;
        }
        let parse_error = normalize_source_for_pipeline(
            row.source_content.as_str(),
            Some(Path::new(row.source_path.as_str())),
        )
        .is_err();
        if parse_error {
            outcome.parse_error_rows += 1;
        }
        if row.prune_candidate || parse_error {
            outcome.deleted_rows += 1;
        }
    }
    Ok(outcome)
}

#[derive(Debug)]
struct AuditedPackageSourceCacheRow {
    rowid: i64,
    package_version: String,
    source_path: String,
    source_content: String,
    missing_identity: bool,
    missing_policy: bool,
    missing_export_specifier: bool,
    stale_policy: bool,
    prune_candidate: bool,
}

fn audited_package_source_cache_rows(
    connection: &Connection,
) -> Result<Vec<AuditedPackageSourceCacheRow>, MatchPackagesError> {
    let has_policy = sqlite_table_has_column(
        connection,
        "package_source_cache",
        "external_import_policy_version",
    )
    .map_err(MatchPackagesError::QueryPackageSources)?;
    let has_export_specifier =
        sqlite_table_has_column(connection, "package_source_cache", "export_specifier")
            .map_err(MatchPackagesError::QueryPackageSources)?;
    let policy_expr = if has_policy {
        "external_import_policy_version"
    } else {
        "0"
    };
    let export_expr = if has_export_specifier {
        "export_specifier"
    } else {
        "''"
    };
    let sql = format!(
        r"
        SELECT rowid, package_name, package_version, entry_path, source_content,
               {policy_expr} AS policy_version,
               {export_expr} AS export_specifier
          FROM package_source_cache
        "
    );
    let mut statement = connection
        .prepare(sql.as_str())
        .map_err(MatchPackagesError::QueryPackageSources)?;
    let rows = statement
        .query_map([], |row| {
            let rowid = row.get::<_, i64>(0)?;
            let package_name = row.get::<_, Option<String>>(1)?.unwrap_or_default();
            let package_version = row.get::<_, Option<String>>(2)?.unwrap_or_default();
            let entry_path = row.get::<_, Option<String>>(3)?.unwrap_or_default();
            let source_content = row.get::<_, Option<String>>(4)?.unwrap_or_default();
            let policy_version = row.get::<_, Option<i64>>(5)?.unwrap_or_default();
            let export_specifier = row.get::<_, Option<String>>(6)?.unwrap_or_default();
            let missing_identity = package_name.trim().is_empty()
                || package_version.trim().is_empty()
                || entry_path.trim().is_empty();
            let missing_policy = !has_policy || policy_version == 0;
            let missing_export_specifier =
                !has_export_specifier || export_specifier.trim().is_empty();
            let stale_policy = missing_policy
                || policy_version != PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION
                || missing_export_specifier;
            let invalid_version = !is_exact_package_version_hint(package_version.as_str());
            Ok(AuditedPackageSourceCacheRow {
                rowid,
                source_path: format!("{package_name}@{package_version}/{entry_path}"),
                package_version,
                source_content,
                missing_identity,
                missing_policy,
                missing_export_specifier,
                stale_policy,
                prune_candidate: missing_identity || invalid_version || stale_policy,
            })
        })
        .map_err(MatchPackagesError::QueryPackageSources)?;
    collect_sqlite_rows(rows).map_err(MatchPackagesError::QueryPackageSources)
}

fn prune_package_source_cache_rows(connection: &mut Connection) -> Result<(), MatchPackagesError> {
    let mut rowids_to_delete = Vec::<i64>::new();
    for row in audited_package_source_cache_rows(connection)? {
        let parse_error = normalize_source_for_pipeline(
            row.source_content.as_str(),
            Some(Path::new(row.source_path.as_str())),
        )
        .is_err();
        if row.prune_candidate || parse_error {
            rowids_to_delete.push(row.rowid);
        }
    }
    if rowids_to_delete.is_empty() {
        return Ok(());
    }
    let transaction = connection
        .transaction()
        .map_err(MatchPackagesError::WritePackageSourceCache)?;
    {
        let mut statement = transaction
            .prepare("DELETE FROM package_source_cache WHERE rowid = ?1")
            .map_err(MatchPackagesError::WritePackageSourceCache)?;
        for rowid in rowids_to_delete {
            statement
                .execute([rowid])
                .map_err(MatchPackagesError::WritePackageSourceCache)?;
        }
    }
    transaction
        .commit()
        .map_err(MatchPackagesError::WritePackageSourceCache)
}
