//! `package-surface-decisions` command: let an Agent resolve ambiguous
//! source-backed package import surfaces through a TSV proposal file while the
//! CLI owns validation and SQLite writes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use reverts_input::sqlite::load_project_rows_from_connection;
use reverts_input::{InputRows, PackageAttributionStatus, PackageSurfaceInput};
use reverts_ir::{is_valid_package_name, split_bare_specifier};
use reverts_observe::{AuditFinding, AuditReport, FindingCode};
use reverts_package::is_accepted_external_attribution;
use reverts_package_matcher::{
    PackageImportSite, VersionedPackageMatchReport, package_import_sites_from_sources,
};
use rusqlite::{Connection, OpenFlags, params};
use semver::Version;

use crate::args::PackageSurfaceDecisionsArgs;
use crate::errors::{CliRunError, MatchPackagesError};
use crate::persistence::package_surfaces::{
    ensure_package_surfaces_table, persist_package_surface,
};
use crate::sqlite_table_exists;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageSurfaceDecisionOutcome {
    pub project_id: u32,
    pub listed: usize,
    pub accepted: usize,
    pub rejected: usize,
    pub blocked: usize,
    pub applied: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PackageSurfaceDecision {
    Accept {
        package_name: String,
        package_version: String,
        export_specifier: String,
        evidence: String,
    },
    Reject {
        package_name: String,
        package_version: Option<String>,
        export_specifier: String,
        evidence: String,
    },
    Block {
        package_name: String,
        package_version: Option<String>,
        export_specifier: String,
        evidence: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LatestPackageSurfaceDecision {
    pub package_name: String,
    pub package_version: Option<String>,
    pub decision: String,
    pub evidence_json: String,
}

impl PackageSurfaceDecision {
    fn package_name(&self) -> &str {
        match self {
            Self::Accept { package_name, .. }
            | Self::Reject { package_name, .. }
            | Self::Block { package_name, .. } => package_name,
        }
    }

    fn package_version(&self) -> Option<&str> {
        match self {
            Self::Accept {
                package_version, ..
            } => Some(package_version),
            Self::Reject {
                package_version, ..
            }
            | Self::Block {
                package_version, ..
            } => package_version.as_deref(),
        }
    }

    fn export_specifier(&self) -> &str {
        match self {
            Self::Accept {
                export_specifier, ..
            }
            | Self::Reject {
                export_specifier, ..
            }
            | Self::Block {
                export_specifier, ..
            } => export_specifier,
        }
    }

    fn evidence(&self) -> &str {
        match self {
            Self::Accept { evidence, .. }
            | Self::Reject { evidence, .. }
            | Self::Block { evidence, .. } => evidence,
        }
    }

    const fn decision(&self) -> &'static str {
        match self {
            Self::Accept { .. } => "accept_surface",
            Self::Reject { .. } => "reject_surface",
            Self::Block { .. } => "block_surface",
        }
    }
}

pub(crate) fn run(args: PackageSurfaceDecisionsArgs) -> Result<(), CliRunError> {
    let outcome =
        package_surface_decisions_from_sqlite(&args).map_err(CliRunError::MatchPackages)?;
    println!(
        "package surface decisions for project {}: {} listed, {} accepted, {} rejected, {} blocked{}",
        outcome.project_id,
        outcome.listed,
        outcome.accepted,
        outcome.rejected,
        outcome.blocked,
        if outcome.applied {
            " (applied)"
        } else {
            " (dry-run)"
        }
    );
    Ok(())
}

pub fn package_surface_decisions_from_sqlite(
    args: &PackageSurfaceDecisionsArgs,
) -> Result<PackageSurfaceDecisionOutcome, MatchPackagesError> {
    let flags = if args.apply {
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
        .busy_timeout(std::time::Duration::from_secs(30))
        .map_err(MatchPackagesError::ConfigureDatabase)?;
    connection
        .execute_batch("PRAGMA foreign_keys = ON")
        .map_err(MatchPackagesError::ConfigureDatabase)?;
    package_surface_decisions_from_connection(&mut connection, args)
}

pub(crate) fn package_surface_decisions_from_connection(
    connection: &mut Connection,
    args: &PackageSurfaceDecisionsArgs,
) -> Result<PackageSurfaceDecisionOutcome, MatchPackagesError> {
    let rows = load_project_rows_from_connection(connection, args.project_id)
        .map_err(MatchPackagesError::LoadInput)?;
    let sites = package_import_sites_from_sources(&rows).map_err(|source| {
        MatchPackagesError::InvalidPackageSurface {
            export_specifier: source.source_file_path,
            message: source.source.to_string(),
        }
    })?;

    let listed = if args.list {
        print_agent_worklist(connection, args.project_id, &rows, &sites)?
    } else {
        0
    };
    let decisions = if let Some(batch) = &args.batch {
        parse_batch(batch)?
    } else {
        Vec::new()
    };
    validate_decisions(
        &decisions,
        &sites,
        &rows.package_surfaces,
        args.replace_existing,
    )?;

    let mut accepted = 0usize;
    let mut rejected = 0usize;
    let mut blocked = 0usize;
    for decision in &decisions {
        match decision {
            PackageSurfaceDecision::Accept { .. } => accepted += 1,
            PackageSurfaceDecision::Reject { .. } => rejected += 1,
            PackageSurfaceDecision::Block { .. } => blocked += 1,
        }
    }

    if args.apply && !decisions.is_empty() {
        ensure_package_surface_decisions_table(connection)?;
        ensure_package_surfaces_table(connection)?;
        let transaction = connection
            .transaction()
            .map_err(MatchPackagesError::WritePackageSurface)?;
        for decision in &decisions {
            persist_decision(&transaction, args.project_id, decision)?;
            if let PackageSurfaceDecision::Accept {
                package_name,
                package_version,
                export_specifier,
                evidence,
            } = decision
            {
                let surface = PackageSurfaceInput::accepted_external(
                    package_name.clone(),
                    package_version.clone(),
                    export_specifier.clone(),
                )
                .with_evidence(agent_surface_evidence(
                    decision.decision(),
                    decision,
                    evidence,
                ));
                persist_package_surface(&transaction, args.project_id, &surface)?;
            }
        }
        transaction
            .commit()
            .map_err(MatchPackagesError::WritePackageSurface)?;
    }

    Ok(PackageSurfaceDecisionOutcome {
        project_id: args.project_id,
        listed,
        accepted,
        rejected,
        blocked,
        applied: args.apply,
    })
}

fn print_agent_worklist(
    connection: &Connection,
    project_id: u32,
    rows: &InputRows,
    sites: &BTreeSet<PackageImportSite>,
) -> Result<usize, MatchPackagesError> {
    let accepted_by_specifier = rows
        .package_surfaces
        .iter()
        .filter(|surface| matches!(surface.status, PackageAttributionStatus::Accepted))
        .map(|surface| (surface.export_specifier.as_str(), surface))
        .collect::<BTreeMap<_, _>>();
    let candidate_versions = package_version_candidates(connection, rows)?;
    let latest_decisions = latest_package_surface_decisions(connection, project_id)?;
    let mut grouped = BTreeMap::<(&str, &str), BTreeSet<&str>>::new();
    for site in sites {
        grouped
            .entry((site.package_name.as_str(), site.specifier.as_str()))
            .or_default()
            .insert(site.source_file_path.as_str());
    }
    println!(
        "package_name\texport_specifier\tsource_files\tstatus\taccepted_version\tcandidate_versions\tagent_decision"
    );
    for ((package_name, specifier), source_files) in grouped {
        let accepted = accepted_by_specifier.get(specifier);
        let candidates = candidate_versions
            .get(package_name)
            .map(|versions| versions.iter().cloned().collect::<Vec<_>>().join(","))
            .unwrap_or_default();
        let agent_decision = latest_decisions
            .get(specifier)
            .map(|decision| decision.decision.as_str())
            .unwrap_or("-");
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            package_name,
            specifier,
            source_files.iter().copied().collect::<Vec<_>>().join(","),
            accepted.map_or("pending", |_| "accepted"),
            accepted
                .and_then(|surface| surface.package_version.as_deref())
                .unwrap_or(""),
            candidates,
            agent_decision,
        );
    }
    Ok(sites
        .iter()
        .map(|site| site.specifier.as_str())
        .collect::<BTreeSet<_>>()
        .len())
}

fn parse_batch(path: &PathBuf) -> Result<Vec<PackageSurfaceDecision>, MatchPackagesError> {
    let content =
        fs::read_to_string(path).map_err(MatchPackagesError::ReadPackageSurfaceDecisionBatch)?;
    let mut decisions = Vec::new();
    for (index, line) in content.lines().enumerate() {
        let line_number = index + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let columns = line.split('\t').collect::<Vec<_>>();
        if columns.first().copied() == Some("operation") {
            continue;
        }
        decisions.push(parse_decision_line(line_number, &columns)?);
    }
    Ok(decisions)
}

fn parse_decision_line(
    line: usize,
    columns: &[&str],
) -> Result<PackageSurfaceDecision, MatchPackagesError> {
    if columns.len() < 5 {
        return invalid_line(
            line,
            "expected OPERATION<TAB>PACKAGE<TAB>VERSION|-<TAB>EXPORT_SPECIFIER<TAB>EVIDENCE",
        );
    }
    let operation = columns[0].trim();
    let package_name = columns[1].trim().to_string();
    let package_version = columns[2].trim();
    let export_specifier = columns[3].trim().to_string();
    let evidence = columns[4..].join("\t").trim().to_string();
    if evidence.is_empty() {
        return invalid_line(line, "evidence must be non-empty");
    }
    let package_version = if package_version == "-" || package_version.is_empty() {
        None
    } else {
        Some(package_version.to_string())
    };
    match operation {
        "accept_surface" => {
            let Some(package_version) = package_version else {
                return invalid_line(line, "accept_surface requires an exact package version");
            };
            Ok(PackageSurfaceDecision::Accept {
                package_name,
                package_version,
                export_specifier,
                evidence,
            })
        }
        "reject_surface" => Ok(PackageSurfaceDecision::Reject {
            package_name,
            package_version,
            export_specifier,
            evidence,
        }),
        "block_surface" => Ok(PackageSurfaceDecision::Block {
            package_name,
            package_version,
            export_specifier,
            evidence,
        }),
        _ => invalid_line(line, "unknown operation"),
    }
}

fn validate_decisions(
    decisions: &[PackageSurfaceDecision],
    sites: &BTreeSet<PackageImportSite>,
    accepted_surfaces: &[PackageSurfaceInput],
    replace_existing: bool,
) -> Result<(), MatchPackagesError> {
    let sites_by_specifier = sites
        .iter()
        .map(|site| (site.specifier.as_str(), site.package_name.as_str()))
        .collect::<BTreeMap<_, _>>();
    let accepted_by_specifier = accepted_surfaces
        .iter()
        .filter(|surface| surface.status == PackageAttributionStatus::Accepted)
        .map(|surface| (surface.export_specifier.as_str(), surface))
        .collect::<BTreeMap<_, _>>();
    let mut accepted_specifiers = BTreeSet::new();
    for decision in decisions {
        let export_specifier = decision.export_specifier();
        if export_specifier.trim().is_empty() {
            return invalid_surface(export_specifier, "export specifier must be non-empty");
        }
        let Some((specifier_package, _subpath)) = split_bare_specifier(export_specifier) else {
            return invalid_surface(
                export_specifier,
                "export specifier is not a bare package specifier",
            );
        };
        if !is_valid_package_name(decision.package_name()) {
            return invalid_surface(export_specifier, "package name is invalid");
        }
        if decision.package_name() != specifier_package {
            return invalid_surface(
                export_specifier,
                "package name must match the bare import specifier package",
            );
        }
        if sites_by_specifier.get(export_specifier).copied() != Some(decision.package_name()) {
            return invalid_surface(
                export_specifier,
                "no matching source import site exists in this project",
            );
        }
        if let Some(version) = decision.package_version()
            && Version::parse(version).is_err()
        {
            return invalid_surface(export_specifier, "package version must be exact semver");
        }
        if matches!(decision, PackageSurfaceDecision::Accept { .. })
            && !accepted_specifiers.insert(export_specifier.to_string())
        {
            return invalid_surface(
                export_specifier,
                "multiple accept_surface rows target the same specifier",
            );
        }
        if let PackageSurfaceDecision::Accept {
            package_name,
            package_version,
            ..
        } = decision
            && let Some(existing) = accepted_by_specifier.get(export_specifier)
            && (existing.package_name != *package_name
                || existing.package_version.as_deref() != Some(package_version.as_str()))
            && !replace_existing
        {
            return invalid_surface(
                export_specifier,
                "conflicting accepted package surface already exists; rerun with --replace-existing after Agent conflict resolution",
            );
        }
    }
    Ok(())
}

pub(crate) fn ensure_package_surface_decisions_table(
    connection: &Connection,
) -> Result<(), MatchPackagesError> {
    connection
        .execute_batch(
            r"
            CREATE TABLE IF NOT EXISTS package_surface_decisions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_id INTEGER NOT NULL,
                package_name TEXT NOT NULL,
                package_version TEXT,
                export_specifier TEXT NOT NULL,
                decision TEXT NOT NULL,
                origin TEXT NOT NULL,
                evidence_json TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_package_surface_decisions_project
                ON package_surface_decisions(project_id, export_specifier);
            ",
        )
        .map_err(MatchPackagesError::WritePackageSurface)
}

pub(crate) fn latest_package_surface_decisions(
    connection: &Connection,
    project_id: u32,
) -> Result<BTreeMap<String, LatestPackageSurfaceDecision>, MatchPackagesError> {
    if !sqlite_table_exists(connection, "package_surface_decisions")
        .map_err(MatchPackagesError::WritePackageSurface)?
    {
        return Ok(BTreeMap::new());
    }
    let mut statement = connection
        .prepare(
            r"
            SELECT export_specifier, package_name, package_version, decision, evidence_json
              FROM package_surface_decisions
             WHERE project_id = ?1
             ORDER BY id ASC
            ",
        )
        .map_err(MatchPackagesError::WritePackageSurface)?;
    let rows = statement
        .query_map([i64::from(project_id)], |row| {
            Ok((
                row.get::<_, String>(0)?,
                LatestPackageSurfaceDecision {
                    package_name: row.get(1)?,
                    package_version: row.get(2)?,
                    decision: row.get(3)?,
                    evidence_json: row.get(4)?,
                },
            ))
        })
        .map_err(MatchPackagesError::WritePackageSurface)?;
    let mut decisions = BTreeMap::new();
    for row in rows {
        let (specifier, decision) = row.map_err(MatchPackagesError::WritePackageSurface)?;
        decisions.insert(specifier, decision);
    }
    Ok(decisions)
}

pub(crate) fn suppress_rejected_or_blocked_surfaces(
    connection: &Connection,
    project_id: u32,
    report: &mut VersionedPackageMatchReport,
) -> Result<usize, MatchPackagesError> {
    let decisions = latest_package_surface_decisions(connection, project_id)?;
    if decisions.is_empty() || report.surfaces.is_empty() {
        return Ok(0);
    }
    let mut suppressed = 0usize;
    let mut audit = AuditReport::default();
    report.surfaces.retain(|surface| {
        let Some(decision) = decisions.get(surface.export_specifier.as_str()) else {
            return true;
        };
        if decision.decision != "reject_surface" && decision.decision != "block_surface" {
            return true;
        }
        suppressed += 1;
        audit.push(
            AuditFinding::warning(
                FindingCode::PackageSurfaceDecisionBlocked,
                format!(
                    "package surface {} was suppressed by Agent {} decision",
                    surface.export_specifier, decision.decision
                ),
            )
            .with_binding(surface.export_specifier.clone()),
        );
        false
    });
    report.audit.extend(audit);
    Ok(suppressed)
}

pub(crate) fn reconcile_cache_surfaces_after_attribution_safety(
    rows: &InputRows,
    report: &mut VersionedPackageMatchReport,
) -> usize {
    let accepted_specifiers = rows
        .package_attributions
        .iter()
        .chain(report.attributions.iter())
        .filter(|attribution| is_accepted_external_attribution(attribution))
        .filter_map(|attribution| {
            Some((
                attribution.package_name.as_str(),
                attribution.package_version.as_deref()?,
                attribution.export_specifier.as_deref()?,
            ))
        })
        .collect::<BTreeSet<_>>();
    let before = report.surfaces.len();
    report.surfaces.retain(|surface| {
        let is_cache_surface = surface
            .evidence
            .as_deref()
            .is_some_and(|evidence| evidence.starts_with("cache-anchored-public-export:"));
        if !is_cache_surface {
            return true;
        }
        let Some(package_version) = surface.package_version.as_deref() else {
            return false;
        };
        accepted_specifiers.contains(&(
            surface.package_name.as_str(),
            package_version,
            surface.export_specifier.as_str(),
        ))
    });
    before.saturating_sub(report.surfaces.len())
}

fn persist_decision(
    connection: &Connection,
    project_id: u32,
    decision: &PackageSurfaceDecision,
) -> Result<(), MatchPackagesError> {
    let evidence = agent_surface_evidence(decision.decision(), decision, decision.evidence());
    connection
        .execute(
            r"
            INSERT INTO package_surface_decisions
                (project_id, package_name, package_version, export_specifier, decision,
                 origin, evidence_json, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5, 'agent', ?6, datetime('now'))
            ",
            params![
                i64::from(project_id),
                decision.package_name(),
                decision.package_version(),
                decision.export_specifier(),
                decision.decision(),
                evidence,
            ],
        )
        .map_err(MatchPackagesError::WritePackageSurface)?;
    Ok(())
}

fn agent_surface_evidence(
    operation: &str,
    decision: &PackageSurfaceDecision,
    evidence: &str,
) -> String {
    serde_json::json!({
        "source": "agent_package_surface_decision",
        "operation": operation,
        "package_name": decision.package_name(),
        "package_version": decision.package_version(),
        "export_specifier": decision.export_specifier(),
        "agent_evidence": evidence,
        "policy_version": 1,
    })
    .to_string()
}

fn package_version_candidates(
    connection: &Connection,
    rows: &InputRows,
) -> Result<BTreeMap<String, BTreeSet<String>>, MatchPackagesError> {
    let mut candidates = BTreeMap::<String, BTreeSet<String>>::new();
    for module in &rows.modules {
        if let (Some(package_name), Some(package_version)) = (
            module.package_name.as_deref(),
            module.package_version.as_deref(),
        ) {
            candidates
                .entry(package_name.to_string())
                .or_default()
                .insert(package_version.to_string());
        }
    }
    for attribution in &rows.package_attributions {
        if attribution.status == PackageAttributionStatus::Accepted
            && let Some(package_version) = attribution.package_version.as_deref()
        {
            candidates
                .entry(attribution.package_name.clone())
                .or_default()
                .insert(package_version.to_string());
        }
    }
    if sqlite_table_exists(connection, "package_source_cache")
        .map_err(MatchPackagesError::WritePackageSurface)?
    {
        let mut statement = connection
            .prepare("SELECT DISTINCT package_name, package_version FROM package_source_cache")
            .map_err(MatchPackagesError::WritePackageSurface)?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(MatchPackagesError::WritePackageSurface)?;
        for row in rows {
            let (package_name, package_version) =
                row.map_err(MatchPackagesError::WritePackageSurface)?;
            candidates
                .entry(package_name)
                .or_default()
                .insert(package_version);
        }
    }
    Ok(candidates)
}

fn invalid_line<T>(line: usize, message: &str) -> Result<T, MatchPackagesError> {
    Err(MatchPackagesError::InvalidPackageSurface {
        export_specifier: format!("line {line}"),
        message: message.to_string(),
    })
}

fn invalid_surface<T>(export_specifier: &str, message: &str) -> Result<T, MatchPackagesError> {
    Err(MatchPackagesError::InvalidPackageSurface {
        export_specifier: export_specifier.to_string(),
        message: message.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use reverts_input::{PackageAttributionInput, ProjectInput};
    use reverts_ir::ModuleId;
    use rusqlite::Connection;
    use tempfile::tempdir;

    fn fixture_db(source: &str) -> (tempfile::TempDir, PathBuf) {
        let temp = tempdir().expect("tempdir");
        let source_path = temp.path().join("bundle.js");
        fs::write(source_path.as_path(), source).expect("source");
        let db = temp.path().join("project.sqlite");
        let connection = Connection::open(db.as_path()).expect("open db");
        connection
            .execute_batch(
                r"
                CREATE TABLE projects (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                CREATE TABLE source_files (id INTEGER PRIMARY KEY, file_path TEXT NOT NULL, file_size INTEGER);
                CREATE TABLE project_files (project_id INTEGER NOT NULL, file_id INTEGER NOT NULL);
                CREATE TABLE modules (
                    id INTEGER PRIMARY KEY, file_id INTEGER, original_name TEXT NOT NULL,
                    semantic_name TEXT, module_category TEXT, package_name TEXT,
                    package_version TEXT, byte_start INTEGER, byte_end INTEGER,
                    created_at TEXT, updated_at TEXT
                );
                CREATE TABLE module_dependencies (module_id INTEGER, dependency_id INTEGER);
                CREATE TABLE symbols (
                    module_id INTEGER, semantic_name TEXT, export_name TEXT,
                    original_name TEXT, scope_level TEXT
                );
                CREATE TABLE package_attributions (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, module_id INTEGER NOT NULL,
                    module_original_name TEXT NOT NULL, package_name TEXT NOT NULL,
                    package_version TEXT, package_subpath TEXT, resolved_file TEXT,
                    export_specifier TEXT, emission_mode TEXT NOT NULL, status TEXT NOT NULL,
                    evidence_json TEXT, rejection_reason TEXT, external_import_policy_version INTEGER,
                    created_at TEXT NOT NULL, updated_at TEXT NOT NULL, UNIQUE(module_id)
                );
                CREATE TABLE package_surfaces (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, project_id INTEGER NOT NULL,
                    package_name TEXT NOT NULL, package_version TEXT NOT NULL,
                    export_specifier TEXT NOT NULL, status TEXT NOT NULL,
                    evidence_json TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                    UNIQUE(project_id, export_specifier)
                );
                INSERT INTO projects (id, name) VALUES (1, 'fixture');
                ",
            )
            .expect("schema");
        connection
            .execute(
                "INSERT INTO source_files (id, file_path, file_size) VALUES (1, ?1, ?2)",
                params![source_path.to_string_lossy().as_ref(), source.len() as i64],
            )
            .expect("source row");
        connection
            .execute(
                "INSERT INTO project_files (project_id, file_id) VALUES (1, 1)",
                [],
            )
            .expect("project file");
        (temp, db)
    }

    #[test]
    fn agent_accept_surface_writes_decision_and_accepted_surface() {
        let (_temp, db) = fixture_db("const ws = require('ws');\n");
        let batch = db.with_extension("tsv");
        fs::write(
            batch.as_path(),
            "accept_surface\tws\t8.18.3\tws\tAPI usage matches ws WebSocket constructor\n",
        )
        .expect("batch");
        let args = PackageSurfaceDecisionsArgs {
            input: db.clone(),
            project_id: 1,
            list: false,
            batch: Some(batch),
            apply: true,
            replace_existing: false,
        };

        let outcome = package_surface_decisions_from_sqlite(&args).expect("apply");

        assert_eq!(outcome.accepted, 1);
        let connection = Connection::open(db.as_path()).expect("open");
        let surface = connection
            .query_row(
                "SELECT package_name, package_version, status FROM package_surfaces WHERE export_specifier='ws'",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)),
            )
            .expect("surface");
        assert_eq!(
            surface,
            (
                "ws".to_string(),
                "8.18.3".to_string(),
                "accepted".to_string()
            )
        );
        let decisions = connection
            .query_row(
                "SELECT COUNT(*) FROM package_surface_decisions",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("decision count");
        assert_eq!(decisions, 1);
    }

    #[test]
    fn agent_accept_surface_requires_matching_import_site() {
        let (_temp, db) = fixture_db("const fs = require('node:fs');\n");
        let batch = db.with_extension("tsv");
        fs::write(
            batch.as_path(),
            "accept_surface\tws\t8.18.3\tws\tevidence\n",
        )
        .expect("batch");
        let args = PackageSurfaceDecisionsArgs {
            input: db,
            project_id: 1,
            list: false,
            batch: Some(batch),
            apply: false,
            replace_existing: false,
        };

        let error = package_surface_decisions_from_sqlite(&args).expect_err("must reject");

        assert!(error.to_string().contains("no matching source import site"));
    }

    #[test]
    fn agent_reject_and_block_surface_write_latest_decision_without_surface() {
        let (_temp, db) = fixture_db("const ws = require('ws');\n");
        let batch = db.with_extension("tsv");
        fs::write(
            batch.as_path(),
            "reject_surface\tws\t-\tws\twrong package candidate\n\
             block_surface\tws\t8.18.3\tws\tAgent found a runtime conflict\n",
        )
        .expect("batch");
        let args = PackageSurfaceDecisionsArgs {
            input: db.clone(),
            project_id: 1,
            list: false,
            batch: Some(batch),
            apply: true,
            replace_existing: false,
        };

        let outcome = package_surface_decisions_from_sqlite(&args).expect("apply");

        assert_eq!(outcome.rejected, 1);
        assert_eq!(outcome.blocked, 1);
        let connection = Connection::open(db.as_path()).expect("open");
        let decisions = latest_package_surface_decisions(&connection, 1).expect("latest");
        assert_eq!(
            decisions
                .get("ws")
                .map(|decision| decision.decision.as_str()),
            Some("block_surface")
        );
        let surfaces = connection
            .query_row("SELECT COUNT(*) FROM package_surfaces", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("surface count");
        assert_eq!(surfaces, 0);
    }

    #[test]
    fn agent_accept_surface_rejects_invalid_rows() {
        let cases = [
            (
                "accept_surface\tws\tlatest\tws\tevidence\n",
                "package version must be exact semver",
            ),
            (
                "accept_surface\tleft-pad\t1.0.0\tws\tevidence\n",
                "package name must match the bare import specifier package",
            ),
            (
                "accept_surface\tws\t1.0.0\tws\tfirst\naccept_surface\tws\t1.0.1\tws\tsecond\n",
                "multiple accept_surface rows target the same specifier",
            ),
        ];
        for (content, message) in cases {
            let (_temp, db) = fixture_db("const ws = require('ws');\n");
            let batch = db.with_extension("tsv");
            fs::write(batch.as_path(), content).expect("batch");
            let args = PackageSurfaceDecisionsArgs {
                input: db,
                project_id: 1,
                list: false,
                batch: Some(batch),
                apply: false,
                replace_existing: false,
            };

            let error = package_surface_decisions_from_sqlite(&args).expect_err("must reject");

            assert!(
                error.to_string().contains(message),
                "{error} should contain {message}"
            );
        }
    }

    #[test]
    fn agent_accept_surface_requires_explicit_replace_for_existing_conflict() {
        let (_temp, db) = fixture_db("const ws = require('ws');\n");
        let connection = Connection::open(db.as_path()).expect("open");
        connection
            .execute(
                r"
                INSERT INTO package_surfaces
                    (project_id, package_name, package_version, export_specifier,
                     status, evidence_json, created_at, updated_at)
                VALUES (1, 'ws', '7.0.0', 'ws', 'accepted', '{}', 'old', 'old')
                ",
                [],
            )
            .expect("seed existing surface");
        drop(connection);
        let batch = db.with_extension("tsv");
        fs::write(
            batch.as_path(),
            "accept_surface\tws\t8.18.3\tws\tAgent resolved version conflict\n",
        )
        .expect("batch");
        let args = PackageSurfaceDecisionsArgs {
            input: db.clone(),
            project_id: 1,
            list: false,
            batch: Some(batch.clone()),
            apply: true,
            replace_existing: false,
        };

        let error = package_surface_decisions_from_sqlite(&args).expect_err("must conflict");
        assert!(
            error
                .to_string()
                .contains("conflicting accepted package surface")
        );

        let replace_args = PackageSurfaceDecisionsArgs {
            replace_existing: true,
            ..args
        };
        package_surface_decisions_from_sqlite(&replace_args).expect("replace should apply");
        let connection = Connection::open(db.as_path()).expect("open");
        let package_version = connection
            .query_row(
                "SELECT package_version FROM package_surfaces WHERE export_specifier='ws'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("surface version");
        assert_eq!(package_version, "8.18.3");
    }

    #[test]
    fn rejected_or_blocked_decision_suppresses_generated_surface() {
        let (_temp, db) = fixture_db("const ws = require('ws');\n");
        let batch = db.with_extension("tsv");
        fs::write(batch.as_path(), "block_surface\tws\t-\tws\tblocked\n").expect("batch");
        let args = PackageSurfaceDecisionsArgs {
            input: db.clone(),
            project_id: 1,
            list: false,
            batch: Some(batch),
            apply: true,
            replace_existing: false,
        };
        package_surface_decisions_from_sqlite(&args).expect("apply");
        let connection = Connection::open(db.as_path()).expect("open");
        let mut report = VersionedPackageMatchReport {
            attributions: Vec::new(),
            surfaces: vec![PackageSurfaceInput::accepted_external("ws", "8.18.3", "ws")],
            matches: Vec::new(),
            version_matches: Vec::new(),
            audit: AuditReport::default(),
        };

        let suppressed =
            suppress_rejected_or_blocked_surfaces(&connection, 1, &mut report).expect("suppress");

        assert_eq!(suppressed, 1);
        assert!(report.surfaces.is_empty());
        assert!(report.audit.has(FindingCode::PackageSurfaceDecisionBlocked));
    }

    #[test]
    fn cache_surface_without_remaining_safe_attribution_is_removed() {
        let rows = InputRows::new(ProjectInput::new(1, "fixture"));
        let mut report = VersionedPackageMatchReport {
            attributions: Vec::new(),
            surfaces: vec![
                PackageSurfaceInput::accepted_external("pkg", "1.0.0", "pkg")
                    .with_evidence("cache-anchored-public-export:pkg"),
            ],
            matches: Vec::new(),
            version_matches: Vec::new(),
            audit: AuditReport::default(),
        };

        let removed = reconcile_cache_surfaces_after_attribution_safety(&rows, &mut report);

        assert_eq!(removed, 1);
        assert!(report.surfaces.is_empty());

        let mut supported_rows = InputRows::new(ProjectInput::new(1, "fixture"));
        supported_rows
            .package_attributions
            .push(PackageAttributionInput::accepted_external(
                ModuleId(1),
                "pkg",
                "1.0.0",
                "pkg",
            ));
        let mut supported_report = VersionedPackageMatchReport {
            attributions: Vec::new(),
            surfaces: vec![
                PackageSurfaceInput::accepted_external("pkg", "1.0.0", "pkg")
                    .with_evidence("cache-anchored-public-export:pkg"),
            ],
            matches: Vec::new(),
            version_matches: Vec::new(),
            audit: AuditReport::default(),
        };

        let removed = reconcile_cache_surfaces_after_attribution_safety(
            &supported_rows,
            &mut supported_report,
        );

        assert_eq!(removed, 0);
        assert_eq!(supported_report.surfaces.len(), 1);
    }
}
