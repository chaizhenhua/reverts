use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use reverts_analyze::rollup::oracle::{OracleConfig, build_oracle};
use reverts_analyze::rollup::projection::project;
use reverts_analyze::rollup::report::summarize;
use reverts_rollup_adapter::db::load_snapshot;
use rusqlite::Connection;

struct Args {
    db: PathBuf,
    json: Option<PathBuf>,
    target_ratio: f64,
    direct_match_floor: f64,
}

fn parse_args() -> Result<Args, String> {
    let mut db: Option<PathBuf> = None;
    let mut json: Option<PathBuf> = None;
    let mut target_ratio = 0.90;
    let mut direct_match_floor = 0.0;

    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--db" => db = Some(PathBuf::from(iter.next().ok_or("--db expects a path")?)),
            "--json" => json = Some(PathBuf::from(iter.next().ok_or("--json expects a path")?)),
            "--target-ratio" => {
                target_ratio = iter
                    .next()
                    .ok_or("--target-ratio expects a number")?
                    .parse()
                    .map_err(|e| format!("--target-ratio: {e}"))?;
            }
            "--direct-match-floor" => {
                direct_match_floor = iter
                    .next()
                    .ok_or("--direct-match-floor expects a number")?
                    .parse()
                    .map_err(|e| format!("--direct-match-floor: {e}"))?;
            }
            "--help" | "-h" => {
                println!(
                    "reverts-rollup-probe [--db PATH] [--json PATH] [--target-ratio N] [--direct-match-floor N]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }

    Ok(Args {
        db: db.unwrap_or_else(default_db_path),
        json,
        target_ratio,
        direct_match_floor,
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
    let conn =
        Connection::open(&args.db).map_err(|e| format!("open {}: {e}", args.db.display()))?;
    let snap = load_snapshot(&conn)?;
    let oracle = build_oracle(
        &snap,
        OracleConfig {
            direct_match_floor: args.direct_match_floor,
        },
    );
    let projections = project(&snap, &oracle);
    let report = summarize(&snap, &projections);

    println!("# Rollup projection ({})", args.db.display());
    println!(
        "package modules: {}  already_accepted: {}  rolled_up: {}  still_rejected: {}",
        report.global.package_modules,
        report.global.already_accepted,
        report.global.rolled_up,
        report.global.still_rejected,
    );
    println!(
        "projected external import ratio: {:.4} (target {:.2})",
        report.global.projected_ratio, args.target_ratio,
    );
    println!();
    println!("# Top packages by module count");
    println!(
        "  {:>5}  {:>5}  {:>5}  {:>5}  {:>6}  name",
        "tot", "acc", "roll", "rej", "ratio%"
    );
    for pkg in report.per_package.iter().take(40) {
        println!(
            "  {:>5}  {:>5}  {:>5}  {:>5}  {:>6.1}  {}",
            pkg.package_modules,
            pkg.already_accepted,
            pkg.rolled_up,
            pkg.still_rejected,
            pkg.projected_ratio * 100.0,
            pkg.package_name,
        );
    }
    if let Some(path) = &args.json {
        fs::write(path, serde_json::to_string_pretty(&report)?)
            .map_err(|e| format!("write {}: {e}", path.display()))?;
    }

    if report.global.projected_ratio + 1e-9 < args.target_ratio {
        eprintln!(
            "projected ratio {:.4} below target {:.2}",
            report.global.projected_ratio, args.target_ratio
        );
        return Ok(ExitCode::from(2));
    }
    Ok(ExitCode::SUCCESS)
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
