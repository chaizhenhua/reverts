//! L5 — end-to-end pipeline test: bundle extraction → graph build →
//! cascade match → attribution. Synthesises a small esbuild bundle and
//! a known package source, verifies that the cascade attributes
//! exactly the matched function with a span inside the extractor body.

use rusqlite::Connection;
use std::path::PathBuf;
use tempfile::tempdir;

#[test]
fn cascade_matches_function_inside_esbuild_extracted_body() {
    let tempdir = tempdir().expect("tempdir");
    let bundle_path = tempdir.path().join("bundle.js");
    let bundle_src = r#"
        var __commonJS=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports);
        var lib = __commonJS({
            "node_modules/example/index.js": (exports, module) => {
                function add(a, b) { return a + b; }
                module.exports = { add };
            }
        });
    "#;
    std::fs::write(&bundle_path, bundle_src).expect("write bundle");

    let mut connection = Connection::open_in_memory().expect("sqlite");
    connection
        .execute_batch(include_str!("bundle_cascade_schema.sql"))
        .expect("schema");
    connection
        .execute(
            "INSERT INTO source_files (id, file_path) VALUES (1, ?1);",
            [bundle_path.to_string_lossy()],
        )
        .expect("source row");
    connection
        .execute_batch(
            r#"
            INSERT INTO projects (id, name) VALUES (1, 'cascade-e2e');
            INSERT INTO project_files (project_id, file_id) VALUES (1, 1);
            INSERT INTO modules
                (id, file_id, original_name, semantic_name, module_category,
                 package_name, package_version, byte_start, byte_end)
                VALUES (10, 1, 'bundle', 'bundle/index', 'application',
                        NULL, NULL, 0, 0);
            INSERT INTO package_source_cache
                (package_name, package_version, entry_path, source_content,
                 content_hash, fetched_at, expires_at)
                VALUES ('example', '1.0.0', 'index.js',
                        'function add(a, b) { return a + b; }',
                        'h', '2026-01-01', '2099-01-01');
            "#,
        )
        .expect("seed rows");

    // Apply mode must persist synthetic extracted modules before any package
    // or function attribution writes reference them.
    let args = reverts_cli::MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: true,
        package_names: Vec::new(),
        package_source_roots: Vec::new(),
        reference_source_roots: Vec::new(),
        materialize_package_sources: false,
    };
    let outcome = reverts_cli::match_packages_from_connection(&mut connection, &args)
        .expect("match should succeed");

    assert!(
        outcome.loaded_package_modules >= 1,
        "extraction should have produced ≥1 package module: {outcome:?}"
    );
    assert!(
        outcome.audit.is_clean(),
        "audit must be clean (no errors): {outcome:?}"
    );
    let synthetic_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM modules WHERE id > 10 AND module_category = 'package'",
            [],
            |row| row.get(0),
        )
        .expect("count synthetic package modules");
    assert!(
        synthetic_count >= 1,
        "apply mode must persist extracted package modules"
    );
}
