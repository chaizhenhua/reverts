//! Thin CLI over [`reverts_rollup_adapter::apply::apply_rollup_projections`].
//! Core logic lives in the library so it can be unit-tested in isolation.

use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;

use reverts_analyze::rollup::oracle::{OracleConfig, build_oracle};
use reverts_rollup_adapter::apply::{ApplyOutcome, apply_rollup_projections, collect_rollups};
use reverts_rollup_adapter::db::load_snapshot;
use rusqlite::{Connection, OpenFlags};

struct Args {
    db: PathBuf,
    dry_run: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut db: Option<PathBuf> = None;
    let mut dry_run = true;
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--db" => db = Some(PathBuf::from(iter.next().ok_or("--db expects a path")?)),
            "--apply" => dry_run = false,
            "--help" | "-h" => {
                println!(
                    "reverts-rollup-apply --db PATH [--apply]\n\
                     dry-run by default; pass --apply to commit.\n\
                     Always stamps the current external-import policy version\n\
                     (defined in reverts_input). There is intentionally no\n\
                     --policy-version override: writing any other value would\n\
                     be silently downgraded by the sqlite loader."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        db: db.unwrap_or_else(default_db_path),
        dry_run,
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

    let snapshot = load_snapshot(&conn)?;
    let oracle = build_oracle(&snapshot, OracleConfig::default());
    let plan = collect_rollups(&snapshot, &oracle);

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
                sample.module_id, sample.top_specifier, sample.package_version
            );
        }
        println!("dry-run complete; pass --apply to commit");
        return Ok(ExitCode::SUCCESS);
    }

    let tx = conn.transaction().map_err(|e| format!("begin tx: {e}"))?;
    let now = unix_timestamp_iso8601();
    let ApplyOutcome {
        attributions_updated,
        surfaces_inserted,
        ..
    } = apply_rollup_projections(&tx, &snapshot, &oracle, &now)?;
    tx.commit().map_err(|e| format!("commit: {e}"))?;
    println!(
        "apply complete: {attributions_updated} attribution row(s), \
         {surfaces_inserted} surface row(s) inserted"
    );
    Ok(ExitCode::SUCCESS)
}

fn unix_timestamp_iso8601() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
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
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}
