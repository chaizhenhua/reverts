use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;

use reverts_analyze::rollup::db::{Snapshot, load_snapshot};
use reverts_analyze::rollup::oracle::{Oracle, OracleConfig, OracleVerdict, build_oracle};
use reverts_analyze::rollup::projection::{ProjectionKind, project};
use rusqlite::{Connection, OpenFlags, params};

struct Args {
    db: PathBuf,
    dry_run: bool,
    policy_version: i64,
}

fn parse_args() -> Result<Args, String> {
    let mut db: Option<PathBuf> = None;
    let mut dry_run = true;
    let mut policy_version: i64 = 2;
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--db" => db = Some(PathBuf::from(iter.next().ok_or("--db expects a path")?)),
            "--apply" => dry_run = false,
            "--policy-version" => {
                policy_version = iter
                    .next()
                    .ok_or("--policy-version expects an integer")?
                    .parse()
                    .map_err(|e| format!("--policy-version: {e}"))?;
            }
            "--help" | "-h" => {
                println!(
                    "reverts-rollup-apply --db PATH [--apply] [--policy-version N]\n\
                     dry-run by default; pass --apply to commit"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        db: db.unwrap_or_else(default_db_path),
        dry_run,
        policy_version,
    })
}

fn default_db_path() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        let mut p = PathBuf::from(home);
        p.push(".reverts");
        p.push(".reverts.db");
        return p;
    }
    PathBuf::from(".reverts.db")
}

fn count_rollups(snap: &Snapshot, oracle: &Oracle) -> Vec<(i64, String, String, String)> {
    let mut out = Vec::new();
    for proj in project(snap, oracle) {
        if let ProjectionKind::RolledUp { top_specifier } = proj.kind
            && let Some(version) = proj.package_version
        {
            out.push((proj.module_id, proj.package_name, version, top_specifier));
        }
    }
    out
}

fn run() -> Result<ExitCode, Box<dyn Error>> {
    let args = parse_args().map_err(|e| -> Box<dyn Error> { e.into() })?;
    let mut conn = Connection::open_with_flags(
        &args.db,
        if args.dry_run {
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX
        },
    )
    .map_err(|e| format!("open {}: {e}", args.db.display()))?;

    let snap = load_snapshot(&conn)?;
    let oracle = build_oracle(&snap, OracleConfig::default());
    let plan = count_rollups(&snap, &oracle);

    // verify oracle verdicts hold so package_surfaces unique constraints make sense
    for (_module_id, name, version, _) in &plan {
        match oracle.lookup(name, version) {
            Some(OracleVerdict::Externalizable { .. }) => {}
            other => {
                return Err(
                    format!("oracle verdict regression for {name}@{version}: {other:?}").into(),
                );
            }
        }
    }

    println!(
        "rollup-apply: db={} mode={} candidates={}",
        args.db.display(),
        if args.dry_run { "DRY-RUN" } else { "APPLY" },
        plan.len()
    );
    if args.dry_run {
        for sample in plan.iter().take(5) {
            println!(
                "  would flip module {} → {} ({})",
                sample.0, sample.3, sample.2
            );
        }
        println!("dry-run complete; pass --apply to commit");
        return Ok(ExitCode::SUCCESS);
    }

    let tx = conn.transaction().map_err(|e| format!("begin tx: {e}"))?;
    let now = unix_timestamp_iso8601();
    let mut stmt = tx.prepare(
        "UPDATE package_attributions
         SET status='accepted',
             emission_mode='external_import',
             export_specifier=?1,
             package_version=?2,
             package_subpath=NULL,
             resolved_file=NULL,
             rejection_reason=NULL,
             external_import_policy_version=?3,
             evidence_json=COALESCE(evidence_json,'{}'),
             updated_at=?4
         WHERE module_id=?5
           AND status='rejected'",
    )?;
    let mut updated = 0usize;
    for (module_id, _name, version, top_specifier) in &plan {
        let n = stmt.execute(params![
            top_specifier,
            version,
            args.policy_version,
            now,
            module_id
        ])?;
        updated += n;
    }
    drop(stmt);
    tx.commit().map_err(|e| format!("commit: {e}"))?;
    println!("apply complete: {updated} rows updated");
    Ok(ExitCode::SUCCESS)
}

fn unix_timestamp_iso8601() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Match the project's existing TEXT timestamps in package_attributions (ISO8601 UTC).
    let (days, secs_of_day) = (secs / 86_400, secs % 86_400);
    let (hh, mm, ss) = (
        secs_of_day / 3600,
        (secs_of_day / 60) % 60,
        secs_of_day % 60,
    );
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    // Howard Hinnant's date algorithm: days since 1970-01-01 → civil (y, m, d).
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}
