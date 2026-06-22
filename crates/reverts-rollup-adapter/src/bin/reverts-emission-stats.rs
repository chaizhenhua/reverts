//! Audit-gate-free probe: load a project bundle and report how many
//! package-category modules the planner would treat as
//! `accepted/external_import` (source-suppressed) vs still-source-emitted.
//! Useful for verifying the effect of `reverts-rollup-apply` on emission
//! independent of whether `generate` passes its audit gate.

use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;

use reverts_input::sqlite::load_project_bundle_from_sqlite;
use reverts_package::accepted_external_module_ids;
use rusqlite::{Connection, OpenFlags};

struct Args {
    db: PathBuf,
    project_ids: Vec<u32>,
}

fn parse_args() -> Result<Args, String> {
    let mut db: Option<PathBuf> = None;
    let mut project_ids: Vec<u32> = Vec::new();
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--db" => db = Some(PathBuf::from(iter.next().ok_or("--db expects a path")?)),
            "--project" => {
                let raw = iter.next().ok_or("--project expects an integer")?;
                project_ids.push(raw.parse().map_err(|e| format!("--project: {e}"))?);
            }
            "--help" | "-h" => {
                println!(
                    "reverts-emission-stats --db PATH [--project N ...]\n\
                     If no --project flags are given, every project in the DB is reported."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        db: db.ok_or("--db is required")?,
        project_ids,
    })
}

fn discover_project_ids(db: &PathBuf) -> Result<Vec<u32>, Box<dyn Error>> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare("SELECT id FROM projects ORDER BY id")?;
    let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row? as u32);
    }
    Ok(out)
}

fn run() -> Result<ExitCode, Box<dyn Error>> {
    let mut args = parse_args().map_err(|e| -> Box<dyn Error> { e.into() })?;
    if args.project_ids.is_empty() {
        args.project_ids = discover_project_ids(&args.db)?;
    }

    println!(
        "{:<8} {:<40} {:>10} {:>10} {:>10} {:>8}",
        "id", "name", "pkg_mods", "ext_attrs", "ext_mods", "pct"
    );
    println!("{}", "-".repeat(94));

    let mut total_pkg = 0usize;
    let mut total_ext = 0usize;

    for pid in &args.project_ids {
        let bundle = match load_project_bundle_from_sqlite(&args.db, *pid) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("project {pid}: load failed: {e}");
                continue;
            }
        };

        let pkg_module_count = bundle
            .modules
            .iter()
            .filter(|m| matches!(m.kind, reverts_ir::ModuleKind::Package))
            .count();
        let ext_attribution_count = bundle
            .package_attributions
            .iter()
            .filter(|a| {
                matches!(a.status, reverts_input::PackageAttributionStatus::Accepted)
                    && matches!(
                        a.emission_mode,
                        reverts_input::PackageEmissionMode::ExternalImport
                    )
            })
            .count();
        // This is the set the planner consumes via PlannerAnalysis::from_program.
        // It's the source-suppression upper bound: modules in this set will not
        // emit application source unless an adapter is required.
        let ext_module_count = accepted_external_module_ids(&bundle.package_attributions).len();
        let project_name = bundle.project.name.clone();
        let pct = if pkg_module_count == 0 {
            0.0
        } else {
            100.0 * ext_module_count as f64 / pkg_module_count as f64
        };
        println!(
            "{:<8} {:<40} {:>10} {:>10} {:>10} {:>7.2}%",
            pid,
            truncate(&project_name, 40),
            pkg_module_count,
            ext_attribution_count,
            ext_module_count,
            pct
        );
        total_pkg += pkg_module_count;
        total_ext += ext_module_count;
    }

    println!("{}", "-".repeat(94));
    let total_pct = if total_pkg == 0 {
        0.0
    } else {
        100.0 * total_ext as f64 / total_pkg as f64
    };
    println!(
        "{:<8} {:<40} {:>10} {:>10} {:>10} {:>7.2}%",
        "TOTAL", "", total_pkg, "", total_ext, total_pct
    );

    Ok(ExitCode::SUCCESS)
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n.saturating_sub(1)])
    }
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
