use reverts_analyze::rollup::db::{Snapshot, load_snapshot};
use reverts_analyze::rollup::oracle::{OracleConfig, OracleVerdict, build_oracle};
use rusqlite::Connection;

fn seed() -> Connection {
    let conn = Connection::open_in_memory().expect("open");
    conn.execute_batch(
        r#"
        CREATE TABLE modules (
            id INTEGER PRIMARY KEY,
            module_category TEXT NOT NULL DEFAULT 'unknown',
            package_name TEXT,
            package_version TEXT
        );
        CREATE TABLE package_attributions (
            id INTEGER PRIMARY KEY,
            module_id INTEGER NOT NULL,
            package_name TEXT NOT NULL,
            package_version TEXT,
            export_specifier TEXT,
            emission_mode TEXT NOT NULL,
            status TEXT NOT NULL,
            evidence_json TEXT,
            rejection_reason TEXT
        );
        CREATE TABLE package_externalization_hints (
            package_name TEXT NOT NULL,
            package_version TEXT NOT NULL,
            entry_path TEXT NOT NULL,
            export_specifier TEXT NOT NULL,
            public_members_json TEXT NOT NULL DEFAULT '[]',
            PRIMARY KEY (package_name, package_version, entry_path, export_specifier)
        );
        INSERT INTO modules(id, module_category, package_name, package_version) VALUES
            (1, 'package', 'lodash', '4.2.0'),
            (2, 'package', 'lodash', '4.2.0'),
            (3, 'package', 'lodash', '4.2.0');
        INSERT INTO package_attributions(module_id, package_name, package_version, export_specifier, emission_mode, status, evidence_json) VALUES
            (1, 'lodash', '4.2.0', 'lodash/property.js', 'external_import', 'accepted',
             '{"external_import_proof":"matched_package_source"}'),
            (2, 'lodash', '4.2.0', NULL, 'application_source', 'rejected',
             '{"matcher":"package_ownership_matcher","ownership_match":{"match_strategy":"dependency_closure_ownership","external_importable":false}}'),
            (3, 'lodash', '4.2.0', NULL, 'application_source', 'rejected',
             '{"matcher":"package_ownership_matcher","ownership_match":{"match_strategy":"dependency_closure_ownership","external_importable":false}}');
        INSERT INTO package_externalization_hints(package_name, package_version, entry_path, export_specifier, public_members_json) VALUES
            ('lodash', '4.2.0', 'lodash.js', 'lodash', '["property","get","set"]');
        "#,
    )
    .expect("seed");
    conn
}

#[test]
fn load_snapshot_returns_three_modules_and_one_hint() {
    let conn = seed();
    let snap: Snapshot = load_snapshot(&conn).expect("load");
    assert_eq!(snap.modules.len(), 3);
    assert_eq!(snap.attributions.len(), 3);
    assert_eq!(snap.hints.len(), 1);
    assert_eq!(snap.hints[0].public_members, vec!["property", "get", "set"]);
}

#[test]
fn oracle_marks_lodash_externalizable_when_one_accepted_and_hint_present() {
    let conn = seed();
    let snap = load_snapshot(&conn).expect("load");
    let oracle = build_oracle(
        &snap,
        OracleConfig {
            direct_match_floor: 0.30,
        },
    );
    let verdict = oracle.lookup("lodash", "4.2.0").expect("present");
    match verdict {
        OracleVerdict::Externalizable {
            top_specifier,
            public_members,
        } => {
            assert_eq!(top_specifier, "lodash");
            assert_eq!(public_members, &["property", "get", "set"]);
        }
        OracleVerdict::NotExternalizable { reason } => panic!("expected ext, got {reason}"),
    }
}

#[test]
fn oracle_rejects_when_hint_missing() {
    let conn = Connection::open_in_memory().expect("open");
    conn.execute_batch(
        r#"
        CREATE TABLE modules (id INTEGER PRIMARY KEY, module_category TEXT, package_name TEXT, package_version TEXT);
        CREATE TABLE package_attributions (
            id INTEGER PRIMARY KEY, module_id INTEGER, package_name TEXT NOT NULL,
            package_version TEXT, export_specifier TEXT, emission_mode TEXT NOT NULL,
            status TEXT NOT NULL, evidence_json TEXT, rejection_reason TEXT
        );
        CREATE TABLE package_externalization_hints (
            package_name TEXT NOT NULL, package_version TEXT NOT NULL,
            entry_path TEXT NOT NULL, export_specifier TEXT NOT NULL,
            public_members_json TEXT NOT NULL DEFAULT '[]',
            PRIMARY KEY (package_name, package_version, entry_path, export_specifier));
        INSERT INTO modules VALUES
            (1,'package','rare','1.0.0'),
            (2,'package','rare','1.0.0'),
            (3,'package','rare','1.0.0'),
            (4,'package','rare','1.0.0');
        INSERT INTO package_attributions(module_id, package_name, package_version, export_specifier, emission_mode, status, evidence_json) VALUES
            (1,'rare','1.0.0','rare/a.js','external_import','accepted','{"external_import_proof":"matched_package_source"}'),
            (2,'rare','1.0.0','rare/b.js','external_import','accepted','{"external_import_proof":"matched_package_source"}');
    "#,
    )
    .unwrap();
    let snap = load_snapshot(&conn).unwrap();
    let oracle = build_oracle(&snap, OracleConfig::default());
    let verdict = oracle.lookup("rare", "1.0.0").expect("present");
    assert!(matches!(verdict, OracleVerdict::NotExternalizable { .. }));
}
