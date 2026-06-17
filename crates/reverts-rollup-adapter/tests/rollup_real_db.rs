use std::path::PathBuf;

use reverts_analyze::rollup::oracle::{OracleConfig, build_oracle};
use reverts_analyze::rollup::projection::project;
use reverts_analyze::rollup::report::summarize;
use reverts_rollup_adapter::db::load_snapshot;
use rusqlite::Connection;

#[test]
fn projected_ratio_meets_target_on_real_db() {
    let path = match std::env::var_os("REVERTS_DB") {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("REVERTS_DB not set; skipping");
            return;
        }
    };
    if !path.exists() {
        eprintln!("{} missing; skipping", path.display());
        return;
    }
    let conn = Connection::open(&path).expect("open");
    let snap = load_snapshot(&conn).expect("load");
    let oracle = build_oracle(&snap, OracleConfig::default());
    let projections = project(&snap, &oracle);
    let report = summarize(&snap, &projections);

    eprintln!(
        "projected={:.4} package_modules={} accepted={} rolled_up={} still_rejected={}",
        report.global.projected_ratio,
        report.global.package_modules,
        report.global.already_accepted,
        report.global.rolled_up,
        report.global.still_rejected,
    );
    assert!(
        report.global.projected_ratio >= 0.90,
        "rollup projection {} below 0.90",
        report.global.projected_ratio
    );
}
