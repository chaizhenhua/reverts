use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use reverts_input::{
    InputRows, ModuleDependencyInput, ModuleDependencyTarget, ModuleInput,
    PACKAGE_ATTRIBUTION_EXTERNAL_IMPORT_POLICY_VERSION, PackageAttributionInput, ProjectInput,
    SourceFileInput,
};
use reverts_ir::{BindingName, ModuleId, ModuleKind};
use reverts_observe::{AuditFinding, AuditReport, FindingCode};
use reverts_package_matcher::{
    ModuleMatchStrategy, PackageMatch, PackageSource, VersionedPackageMatchReport,
    package_source_normalized_hash,
};
use reverts_pipeline::{
    EmittedAsset, EmittedFile, RuntimeDependency, RuntimeSetterMigrationBlockerReason,
    RuntimeSetterMigrationBlockerReport,
};
use rusqlite::{Connection, params};

use super::commands::generate_project::{checked_output_path, write_emitted_project};
use super::commands::runtime_inventory::{
    RuntimeSourceSpanOwner, package_source_blocker_report_from_files,
    runtime_emitted_setter_blockers_from_files, runtime_inventory_counts_from_files,
    runtime_inventory_project_selections, runtime_line_attribution_from_files,
    runtime_module_owner_label, runtime_original_name_owners_by_binding,
    runtime_source_span_owner_label_for_range,
};
use super::input_externalization::{
    promote_detected_package_modules, promote_verified_externalization_hints,
};
use super::persistence::attributions::{
    externalization_chain_proofs, filter_unsafe_interpackage_external_attributions,
    package_source_elimination_stats_for_report, source_eliminated_package_modules_for_report,
};
use super::pkg_sources::{
    collect_local_package_sources, json_package_source_module, local_package_metadata,
};
use super::{
    CliCommand, CliError, ExtractAssetsArgs, GenerateProjectV2Args, HelpTopic, MatchPackagesArgs,
    MatchPackagesError, PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION,
    PackageExternalizationHintsArgs, PackageVersionDiagnosticsArgs, PackageVersionResolutionPlan,
    RuntimeInventoryArgs, best_matching_package_version_by_binary_search, dedup_audit_report,
    filter_package_sources_to_best_build_variants, filter_package_sources_to_relevant_path_hints,
    help_text, load_package_sources, match_packages_from_connection,
    network_package_version_resolution_hints, package_export_specifier,
    package_externalization_hints_from_connection, package_graph_component_scope,
    package_version_hints_for_materialization, package_version_resolution_evidence,
    package_versions_by_module, persist_package_source_cache,
    promote_package_sources_with_externalization_hints,
    remove_package_attributions_for_revalidation,
    resolve_package_version_hints_to_available_sources, run, stable_hash,
    stale_cache_version_hints_for_materialization, stale_package_source_cache_versions,
    version_text,
};

#[test]
fn parses_generate_project_v2_paths_without_external_process() {
    let args = GenerateProjectV2Args::parse([
        "generate-project-v2".to_string(),
        "--input".to_string(),
        "input.db".to_string(),
        "--project-id".to_string(),
        "13495".to_string(),
        "--output".to_string(),
        "out".to_string(),
    ])
    .expect("args should parse");

    assert_eq!(args.input, PathBuf::from("input.db"));
    assert_eq!(args.project_id, 13495);
    assert_eq!(args.output, PathBuf::from("out"));
}

#[test]
fn project_id_must_be_positive_integer() {
    let error = GenerateProjectV2Args::parse([
        "--input".to_string(),
        "input.db".to_string(),
        "--project-id".to_string(),
        "0".to_string(),
        "--output".to_string(),
        "out".to_string(),
    ]);

    assert!(matches!(error, Err(CliError::InvalidProjectId(value)) if value == "0"));
}

#[test]
fn parses_match_packages_command_without_version_suffix() {
    let args = MatchPackagesArgs::parse([
        "match-packages".to_string(),
        "--input".to_string(),
        "input.db".to_string(),
        "--project-id".to_string(),
        "13495".to_string(),
        "--package-name".to_string(),
        "pkg".to_string(),
        "--package-source-root".to_string(),
        "node_modules".to_string(),
        "--materialize-package-sources".to_string(),
        "--apply".to_string(),
    ])
    .expect("args should parse");

    assert_eq!(args.input, PathBuf::from("input.db"));
    assert_eq!(args.project_id, 13495);
    assert_eq!(args.package_names, vec!["pkg"]);
    assert_eq!(
        args.package_source_roots,
        vec![PathBuf::from("node_modules")]
    );
    assert!(args.materialize_package_sources);
    assert!(args.apply);

    let old_command = CliCommand::parse(["match-packages-v2".to_string()]);
    assert!(
        matches!(old_command, Err(CliError::UnknownCommand(command)) if command == "match-packages-v2")
    );
}

#[test]
fn parses_package_externalization_hints_command() {
    let args = PackageExternalizationHintsArgs::parse([
        "package-externalization-hints".to_string(),
        "--input".to_string(),
        "input.db".to_string(),
        "--package-name".to_string(),
        "pkg".to_string(),
        "--limit".to_string(),
        "50".to_string(),
        "--apply".to_string(),
    ])
    .expect("args should parse");

    assert_eq!(args.input, PathBuf::from("input.db"));
    assert_eq!(args.package_names, vec!["pkg"]);
    assert_eq!(args.limit, Some(50));
    assert!(args.apply);

    let command = CliCommand::parse([
        "package-externalization-hints".to_string(),
        "--input".to_string(),
        "input.db".to_string(),
    ])
    .expect("command should parse");
    assert!(matches!(
        command,
        CliCommand::PackageExternalizationHints(parsed) if parsed.input.as_path() == Path::new("input.db")
    ));
}

#[test]
fn parses_package_version_diagnostics_command() {
    let args = PackageVersionDiagnosticsArgs::parse([
        "package-version-diagnostics".to_string(),
        "--input".to_string(),
        "input.db".to_string(),
        "--project-id".to_string(),
        "1".to_string(),
        "--package-name".to_string(),
        "@opentelemetry/api".to_string(),
        "--package-source-root".to_string(),
        "node_modules".to_string(),
        "--materialize-package-sources".to_string(),
        "--top".to_string(),
        "3".to_string(),
    ])
    .expect("args should parse");

    assert_eq!(args.input, PathBuf::from("input.db"));
    assert_eq!(args.project_id, 1);
    assert_eq!(args.package_names, vec!["@opentelemetry/api"]);
    assert_eq!(
        args.package_source_roots,
        vec![PathBuf::from("node_modules")]
    );
    assert!(args.materialize_package_sources);
    assert_eq!(args.top, 3);

    let command = CliCommand::parse([
        "package-version-diagnostics".to_string(),
        "--input".to_string(),
        "input.db".to_string(),
        "--project-id".to_string(),
        "1".to_string(),
    ])
    .expect("command should parse");
    assert!(
        matches!(command, CliCommand::PackageVersionDiagnostics(parsed) if parsed.project_id == 1)
    );
}

#[test]
fn parses_extract_assets_command() {
    let args = ExtractAssetsArgs::parse([
        "extract-assets".to_string(),
        "--input".to_string(),
        "input.db".to_string(),
        "--project-id".to_string(),
        "13495".to_string(),
        "--asset-root".to_string(),
        "dist".to_string(),
        "--asset-root".to_string(),
        "vendor".to_string(),
        "--apply".to_string(),
    ])
    .expect("args should parse");

    assert_eq!(args.input, PathBuf::from("input.db"));
    assert_eq!(args.project_id, 13495);
    assert_eq!(
        args.asset_roots,
        vec![PathBuf::from("dist"), PathBuf::from("vendor")]
    );
    assert!(args.apply);
}

#[test]
fn parses_runtime_inventory_command() {
    let args = RuntimeInventoryArgs::parse([
        "runtime-inventory".to_string(),
        "--input".to_string(),
        "input.db".to_string(),
        "--all-projects".to_string(),
        "--limit".to_string(),
        "25".to_string(),
        "--newest".to_string(),
        "--max-source-bytes".to_string(),
        "1000000".to_string(),
        "--setter-blockers".to_string(),
        "--runtime-attribution".to_string(),
        "--package-source-blockers".to_string(),
    ])
    .expect("args should parse");

    assert_eq!(args.input, PathBuf::from("input.db"));
    assert_eq!(args.project_id, None);
    assert!(args.all_projects);
    assert_eq!(args.limit, Some(25));
    assert!(args.newest);
    assert_eq!(args.max_source_bytes, Some(1_000_000));
    assert!(args.setter_blockers);
    assert!(args.runtime_attribution);
    assert!(args.package_source_blockers);

    let command = CliCommand::parse([
        "runtime-inventory".to_string(),
        "--input".to_string(),
        "input.db".to_string(),
        "--project-id".to_string(),
        "13495".to_string(),
    ])
    .expect("command should parse");
    assert!(
        matches!(command, CliCommand::RuntimeInventory(parsed) if parsed.project_id == Some(13495))
    );
}

#[test]
fn parses_top_level_help_and_version_without_required_command_args() {
    assert_eq!(
        CliCommand::parse(Vec::<String>::new()).expect("empty args should show help"),
        CliCommand::Help(HelpTopic::TopLevel)
    );
    assert_eq!(
        CliCommand::parse(["--help".to_string()]).expect("top-level help should parse"),
        CliCommand::Help(HelpTopic::TopLevel)
    );
    assert_eq!(
        CliCommand::parse(["-h".to_string()]).expect("short help should parse"),
        CliCommand::Help(HelpTopic::TopLevel)
    );
    assert_eq!(
        CliCommand::parse(["help".to_string()]).expect("help command should parse"),
        CliCommand::Help(HelpTopic::TopLevel)
    );
    assert_eq!(
        CliCommand::parse(["--version".to_string()]).expect("version should parse"),
        CliCommand::Version
    );
    assert_eq!(
        CliCommand::parse(["-V".to_string()]).expect("short version should parse"),
        CliCommand::Version
    );
    assert_eq!(
        CliCommand::parse(["version".to_string()]).expect("version command should parse"),
        CliCommand::Version
    );
}

#[test]
fn parses_command_specific_help_without_running_command() {
    assert_eq!(
        CliCommand::parse(["generate-project-v2".to_string(), "--help".to_string()])
            .expect("generate help should parse"),
        CliCommand::Help(HelpTopic::GenerateProjectV2)
    );
    assert_eq!(
        CliCommand::parse(["match-packages".to_string(), "help".to_string()])
            .expect("match help should parse"),
        CliCommand::Help(HelpTopic::MatchPackages)
    );
    assert_eq!(
        CliCommand::parse(["help".to_string(), "extract-assets".to_string()])
            .expect("extract help should parse"),
        CliCommand::Help(HelpTopic::ExtractAssets)
    );
    assert_eq!(
        CliCommand::parse(["runtime-inventory".to_string(), "--help".to_string()])
            .expect("inventory help should parse"),
        CliCommand::Help(HelpTopic::RuntimeInventory)
    );
}

#[test]
fn help_and_version_commands_return_ok() {
    run(["--help".to_string()]).expect("top-level help should not require a database");
    run(["help".to_string(), "extract-assets".to_string()])
        .expect("command help should not require a database");
    run(["--version".to_string()]).expect("version should not require a database");
}

#[test]
fn help_text_documents_commands_and_options() {
    assert!(help_text(HelpTopic::TopLevel).contains("extract-assets"));
    assert!(help_text(HelpTopic::GenerateProjectV2).contains("--output <DIR>"));
    assert!(help_text(HelpTopic::MatchPackages).contains("--package-name <NAME>"));
    assert!(help_text(HelpTopic::MatchPackages).contains("--package-source-root <DIR>"));
    assert!(help_text(HelpTopic::MatchPackages).contains("--materialize-package-sources"));
    assert!(help_text(HelpTopic::MatchPackagesReport).contains("source_eliminated"));
    assert!(help_text(HelpTopic::PackageVersionDiagnostics).contains("--top <N>"));
    assert!(help_text(HelpTopic::ExtractAssets).contains("--asset-root <DIR-OR-BUN-EXE>"));
    assert!(help_text(HelpTopic::RuntimeInventory).contains("--all-projects"));
    assert!(version_text().starts_with("reverts-cli "));
}

#[test]
fn runtime_inventory_counts_runtime_helpers_and_internal_names() {
    let files = vec![
        EmittedFile {
            path: "modules/runtime/source-1-helpers.ts".to_string(),
            source: "// @ts-nocheck\nexport { X } from '../real.js';\nfunction __reverts_set_X(value) { return X = value; }function __reverts_set_Y(value) { return Y = value; }\n".to_string(),
        },
        EmittedFile {
            path: "modules/consumer.ts".to_string(),
            source: "import { __reverts_set_X } from './runtime/source-1-helpers.js';\n__reverts_set_X(1);\n".to_string(),
        },
    ];

    let counts = runtime_inventory_counts_from_files(&files);

    assert_eq!(counts.files, 2);
    assert_eq!(counts.runtime_files, 1);
    assert_eq!(counts.runtime_lines, 3);
    assert_eq!(counts.runtime_import_statements, 1);
    assert_eq!(counts.runtime_reexport_statements, 1);
    assert_eq!(counts.setter_function_definitions, 2);
    assert_eq!(counts.setter_import_statements, 1);
    assert_eq!(counts.setter_occurrences, 4);
    assert_eq!(counts.reverts_internal_occurrences, 4);
    assert_eq!(counts.named_import_statements, 1);
    assert_eq!(counts.named_export_statements, 1);
}

#[test]
fn runtime_inventory_counts_only_real_setter_declarations() {
    let files = vec![EmittedFile {
        path: "modules/runtime/source-1-helpers.ts".to_string(),
        source: "// function __reverts_set_comment(value) {}\n\
                 const text = 'function __reverts_set_string(value) {}';\n\
                 function __reverts_set_X(value) { return X = value; }function __reverts_set_Y(value) { return Y = value; }\n"
            .to_string(),
    }];

    let counts = runtime_inventory_counts_from_files(&files);

    assert_eq!(counts.setter_function_definitions, 2);
}

#[test]
fn package_source_blocker_report_groups_preserved_package_source() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "package.js",
        Some("var init = 1;".to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(10),
            "init",
            "modules/pkg.ts",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(11),
            "adapter",
            "modules/adapter.ts",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1),
    );
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(ModuleId(10), "pkg", "1.0.0", "pkg/internal")
            .with_resolved_file("forced-external:semantic-source:pkg@1.0.0/internal.js"),
    );
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(ModuleId(11), "pkg", "1.0.0", "pkg")
            .with_resolved_file("exact-hint:pkg@1.0.0:quality=trusted"),
    );
    let input = reverts_input::InputBundle::from_rows(rows).expect("rows should be valid");
    let files = vec![
        EmittedFile {
            path: "modules/pkg.ts".to_string(),
            source: "var pkgInit = 1;\nexport { pkgInit };".to_string(),
        },
        EmittedFile {
            path: "modules/adapter.ts".to_string(),
            source: "import * as external_pkg from 'pkg';\nfunction adapter() { return Object.prototype.hasOwnProperty.call(external_pkg, \"default\") ? external_pkg.default : external_pkg; }\nexport { adapter };".to_string(),
        },
    ];

    let report = package_source_blocker_report_from_files(&input, &files);

    assert_eq!(report.source_package_files, 1);
    assert_eq!(report.items[0].module_id, 10);
    assert_eq!(report.items[0].reason, "semantic_source_suggestion");
    assert_eq!(
        report
            .by_package
            .get("pkg@1.0.0")
            .map(|bucket| bucket.files),
        Some(1)
    );
}

#[test]
fn verified_externalization_hints_promote_dependency_free_attributions() {
    let package_source =
        "var init = (() => { let cached; return () => cached ||= { ok: true }; })();";
    let normalized_source_hash =
        package_source_normalized_hash("pkg@1.0.0/index.js", package_source)
            .expect("source should normalize");
    let connection = Connection::open_in_memory().expect("open db");
    connection
        .execute(
            r"
            CREATE TABLE package_externalization_hints (
                package_name TEXT NOT NULL,
                package_version TEXT NOT NULL,
                entry_path TEXT NOT NULL,
                export_specifier TEXT NOT NULL,
                normalized_source_hash TEXT NOT NULL
            )
            ",
            [],
        )
        .expect("create hints");
    connection
        .execute(
            r"
            INSERT INTO package_externalization_hints
                (package_name, package_version, entry_path, export_specifier, normalized_source_hash)
            VALUES ('pkg', '1.0.0', 'index.js', 'pkg', ?1)
            ",
            params![normalized_source_hash],
        )
        .expect("insert hint");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "package.js",
        Some(package_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(10),
            "init",
            "modules/pkg.ts",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1),
    );
    rows.package_attributions
        .push(PackageAttributionInput::accepted_external(
            ModuleId(10),
            "pkg",
            "1.0.0",
            "pkg",
        ));
    let mut input = reverts_input::InputBundle::from_rows(rows).expect("rows should be valid");

    let promoted =
        promote_verified_externalization_hints(&connection, &mut input).expect("promote hints");

    assert_eq!(promoted, 1);
    assert_eq!(
        input.package_attributions[0].resolved_file.as_deref(),
        Some("normalized-source-export:pkg@1.0.0/index.js")
    );
}

/// Materialized-manifest map for the given package names, each with a permissive
/// `package.json` (a `main`, no `exports`) so every detected specifier — bare
/// root or subpath — counts as public. Tests needing a restrictive exports map
/// build their own map.
fn materialized(names: &[&str]) -> std::collections::BTreeMap<String, (String, bool)> {
    names
        .iter()
        .map(|name| {
            (
                (*name).to_string(),
                (format!(r#"{{"name":"{name}","main":"./index.js"}}"#), true),
            )
        })
        .collect()
}

#[test]
fn detected_package_modules_skip_unmaterialized_packages() {
    // A package the matcher never materialized (e.g. a 404 version baked into
    // the recovered bundle) must NOT be promoted to an external import: that
    // would emit a bare import plus an uninstallable `package.json` dependency.
    // It stays a rejected source attribution and is vendored instead.
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "sha",
        "@smithy/sha256-js/index",
        "@smithy/sha256-js",
        Some("2.3.0".to_string()),
    ));
    rows.package_attributions
        .push(PackageAttributionInput::rejected_source(
            ModuleId(10),
            "@smithy/sha256-js",
            "package matcher did not produce an accepted attribution for this package",
        ));
    let mut input = reverts_input::InputBundle::from_rows(rows).expect("rows should be valid");

    // Only `other-pkg` was materialized; @smithy/sha256-js was never fetched.
    let promoted = promote_detected_package_modules(&mut input, &materialized(&["other-pkg"]));

    assert_eq!(promoted, 0, "unmaterialized package must not be promoted");
    assert_eq!(
        input.package_attributions[0].status,
        reverts_input::PackageAttributionStatus::Rejected
    );
}

#[test]
fn detected_package_modules_skip_non_public_specifier() {
    // The module's semantic path maps to the subpath `pkg/internal`, but pkg's
    // `exports` map only exposes `.`. Externalizing to `pkg/internal` would emit
    // a bare import that crashes with ERR_PACKAGE_PATH_NOT_EXPORTED (the real
    // `axios/exports` bug). The module must stay vendored (rejected).
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "internal",
        "pkg/internal",
        "pkg",
        Some("1.0.0".to_string()),
    ));
    rows.package_attributions
        .push(PackageAttributionInput::rejected_source(
            ModuleId(10),
            "pkg",
            "package matcher did not produce an accepted attribution for this package",
        ));
    let mut input = reverts_input::InputBundle::from_rows(rows).expect("rows should be valid");

    // pkg is materialized but its exports map only exposes the bare root.
    let manifests = std::collections::BTreeMap::from([(
        "pkg".to_string(),
        (
            r#"{"name":"pkg","exports":{".":"./index.js"}}"#.to_string(),
            false,
        ),
    )]);
    let promoted = promote_detected_package_modules(&mut input, &manifests);

    assert_eq!(promoted, 0, "non-public specifier must not be externalized");
    assert_eq!(
        input.package_attributions[0].status,
        reverts_input::PackageAttributionStatus::Rejected
    );
}

#[test]
fn detected_package_modules_promote_rejected_source_attributions() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "init",
        "pkg/internal/init",
        "pkg",
        Some("1.2.3".to_string()),
    ));
    rows.package_attributions
        .push(PackageAttributionInput::rejected_source(
            ModuleId(10),
            "pkg",
            "no cached package source was available for this package",
        ));
    let mut input = reverts_input::InputBundle::from_rows(rows).expect("rows should be valid");

    let promoted = promote_detected_package_modules(&mut input, &materialized(&["pkg"]));

    let attribution = &input.package_attributions[0];
    assert_eq!(promoted, 1);
    assert_eq!(
        attribution.status,
        reverts_input::PackageAttributionStatus::Accepted
    );
    assert_eq!(
        attribution.emission_mode,
        reverts_input::PackageEmissionMode::ExternalImport
    );
    assert_eq!(attribution.package_version.as_deref(), Some("1.2.3"));
    assert_eq!(
        attribution.export_specifier.as_deref(),
        Some("pkg/internal/init")
    );
    assert_eq!(attribution.subpath.as_deref(), Some("internal/init"));
    assert_eq!(
        attribution.resolved_file.as_deref(),
        Some("forced-external:semantic-path:pkg@1.2.3/pkg/internal/init")
    );
    assert_eq!(attribution.rejection_reason, None);
}

#[test]
fn detected_package_modules_use_scoped_package_alias_subpaths() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "client",
        "sdk/client",
        "@scope/sdk",
        Some("2.0.0".to_string()),
    ));
    rows.package_attributions
        .push(PackageAttributionInput::rejected_source(
            ModuleId(10),
            "@scope/sdk",
            "package version search found no usable evidence",
        ));
    let mut input = reverts_input::InputBundle::from_rows(rows).expect("rows should be valid");

    let promoted = promote_detected_package_modules(&mut input, &materialized(&["@scope/sdk"]));

    assert_eq!(promoted, 1);
    assert_eq!(
        input.package_attributions[0].export_specifier.as_deref(),
        Some("@scope/sdk/client")
    );
}

#[test]
fn detected_package_modules_infer_missing_version_from_package_group() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "known",
        "modules/10-pkg/known.ts",
        "pkg",
        Some("1.2.3".to_string()),
    ));
    rows.package_attributions
        .push(PackageAttributionInput::rejected_source(
            ModuleId(10),
            "pkg",
            "source-only",
        ));
    rows.modules.push(ModuleInput::package(
        ModuleId(11),
        "minified",
        "modules/11-minified.ts",
        "pkg",
        None,
    ));
    rows.package_attributions
        .push(PackageAttributionInput::rejected_source(
            ModuleId(11),
            "pkg",
            "no cached package source was available for this package",
        ));
    let mut input = reverts_input::InputBundle::from_rows(rows).expect("rows should be valid");

    let promoted = promote_detected_package_modules(&mut input, &materialized(&["pkg"]));

    assert_eq!(promoted, 2);
    assert_eq!(
        input.package_attributions[1].package_version.as_deref(),
        Some("1.2.3")
    );
    assert_eq!(
        input.package_attributions[1].export_specifier.as_deref(),
        Some("pkg")
    );
}

#[test]
fn verified_externalization_hints_skip_loading_when_no_promotable_attributions() {
    let connection = Connection::open_in_memory().expect("open db");
    connection
        .execute(
            "CREATE TABLE package_externalization_hints (unexpected TEXT NOT NULL)",
            [],
        )
        .expect("create irrelevant malformed hints table");
    let rows = InputRows::new(ProjectInput::new(1, "fixture"));
    let mut input = reverts_input::InputBundle::from_rows(rows).expect("rows should be valid");

    let promoted =
        promote_verified_externalization_hints(&connection, &mut input).expect("promote hints");

    assert_eq!(promoted, 0);
}

#[test]
fn verified_externalization_hints_promote_stable_normalization_alternates() {
    let package_source = "function add(a,b){return a+b;}\nexports.add = add;";
    let module_source = "function add(a,b){return a+b;}";
    let normalized_source_hash =
        package_source_normalized_hash("pkg@1.0.0/lib/add.js", package_source)
            .expect("source should normalize");
    let connection = Connection::open_in_memory().expect("open db");
    connection
        .execute(
            r"
            CREATE TABLE package_externalization_hints (
                package_name TEXT NOT NULL,
                package_version TEXT NOT NULL,
                entry_path TEXT NOT NULL,
                export_specifier TEXT NOT NULL,
                normalized_source_hash TEXT NOT NULL
            )
            ",
            [],
        )
        .expect("create hints");
    connection
        .execute(
            r"
            CREATE TABLE package_source_cache (
                package_name TEXT NOT NULL,
                package_version TEXT NOT NULL,
                entry_path TEXT NOT NULL,
                source_content TEXT NOT NULL
            )
            ",
            [],
        )
        .expect("create cache");
    connection
        .execute(
            r"
            INSERT INTO package_externalization_hints
                (package_name, package_version, entry_path, export_specifier, normalized_source_hash)
            VALUES ('pkg', '1.0.0', 'lib/add.js', 'pkg/add', ?1)
            ",
            params![normalized_source_hash],
        )
        .expect("insert hint");
    connection
        .execute(
            r"
            INSERT INTO package_source_cache
                (package_name, package_version, entry_path, source_content)
            VALUES ('pkg', '1.0.0', 'lib/add.js', ?1)
            ",
            params![package_source],
        )
        .expect("insert cache source");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "module.js",
        Some(module_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(10),
            "add",
            "modules/pkg/add.ts",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1),
    );
    rows.package_attributions
        .push(PackageAttributionInput::accepted_external(
            ModuleId(10),
            "pkg",
            "1.0.0",
            "pkg/add",
        ));
    let mut input = reverts_input::InputBundle::from_rows(rows).expect("rows should be valid");

    let promoted =
        promote_verified_externalization_hints(&connection, &mut input).expect("promote hints");

    assert_eq!(promoted, 1);
    assert_eq!(
        input.package_attributions[0].resolved_file.as_deref(),
        Some("normalized-source-export:pkg@1.0.0/lib/add.js")
    );
}

#[test]
fn verified_externalization_hints_promote_public_member_proofs() {
    let package_source = "exports.PublicClient = class PublicClient {};";
    let normalized_source_hash =
        package_source_normalized_hash("pkg@1.0.0/index.js", package_source)
            .expect("source should normalize");
    let connection = Connection::open_in_memory().expect("open db");
    connection
        .execute(
            r"
            CREATE TABLE package_externalization_hints (
                package_name TEXT NOT NULL,
                package_version TEXT NOT NULL,
                entry_path TEXT NOT NULL,
                export_specifier TEXT NOT NULL,
                normalized_source_hash TEXT NOT NULL,
                public_members_json TEXT NOT NULL
            )
            ",
            [],
        )
        .expect("create hints");
    connection
        .execute(
            r"
            INSERT INTO package_externalization_hints
                (package_name, package_version, entry_path, export_specifier,
                 normalized_source_hash, public_members_json)
            VALUES ('pkg', '1.0.0', 'index.js', 'pkg', ?1, ?2)
            ",
            params![normalized_source_hash, "[\"PublicClient\"]"],
        )
        .expect("insert hint");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "module.js",
        Some("var packageInit = U((exports, module) => { exports.PublicClient = class LocalClient {}; });".to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(10),
            "packageInit",
            "modules/pkg.ts",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1),
    );
    rows.package_attributions
        .push(PackageAttributionInput::accepted_external(
            ModuleId(10),
            "pkg",
            "1.0.0",
            "pkg",
        ));
    let mut input = reverts_input::InputBundle::from_rows(rows).expect("rows should be valid");

    let promoted =
        promote_verified_externalization_hints(&connection, &mut input).expect("promote hints");

    assert_eq!(promoted, 1);
    assert_eq!(
        input.package_attributions[0].resolved_file.as_deref(),
        Some("forced-external:export-members:public-members:PublicClient:pkg@1.0.0/index.js")
    );
}

#[test]
fn runtime_emitted_setter_blockers_count_batched_setter_declarations() {
    let files = vec![EmittedFile {
        path: "modules/runtime/source-1-helpers.ts".to_string(),
        source: "function __reverts_set_X(value) { return X = value; }function __reverts_set_Y(value) { return Y = value; }\n".to_string(),
    }];
    let mut report = RuntimeSetterMigrationBlockerReport {
        total_bindings: 2,
        ..Default::default()
    };
    report.add_reason_with_sub(
        1,
        BindingName::new("X"),
        RuntimeSetterMigrationBlockerReason::ReaderNonSnippetUse,
        Some("lazy_init_cycle_import"),
    );
    report.add_reason(
        1,
        BindingName::new("Y"),
        RuntimeSetterMigrationBlockerReason::RuntimeNonSnippetRead,
    );

    let emitted = runtime_emitted_setter_blockers_from_files(&files, &report);

    assert_eq!(emitted.total_bindings, 2);
    assert_eq!(emitted.blocked_bindings, 2);
    assert_eq!(
        emitted
            .reasons
            .get(&RuntimeSetterMigrationBlockerReason::ReaderNonSnippetUse),
        Some(&1)
    );
    assert_eq!(
        emitted
            .reasons
            .get(&RuntimeSetterMigrationBlockerReason::RuntimeNonSnippetRead),
        Some(&1)
    );
    assert_eq!(
        emitted.sub_reasons.get(&(
            RuntimeSetterMigrationBlockerReason::ReaderNonSnippetUse,
            "lazy_init_cycle_import"
        )),
        Some(&1)
    );
}

#[test]
fn runtime_line_attribution_reports_runtime_lines_by_kind_and_binding() {
    let files = vec![
        EmittedFile {
            path: "modules/runtime/source-1-helpers.ts".to_string(),
            source: "import { dep } from '../dep.js';\n\
                     var cached = lazyValue(() => ({ dep }));\n\
                     function __reverts_set_cached(value) { cached = value; return value; }\n\
                     function run() {\n  return cached();\n}\n\
                     class Box {}\n\
                     export { cached, run };\n"
                .to_string(),
        },
        EmittedFile {
            path: "modules/consumer.ts".to_string(),
            source: "import { run } from './runtime/source-1-helpers.js';\nrun();\n".to_string(),
        },
    ];

    let package_ownership = BTreeMap::from([
        ((1, "cached".to_string()), "fixture@1.0.0".to_string()),
        ((1, "run".to_string()), "<application>".to_string()),
        ((1, "Box".to_string()), "ui@2.0.0".to_string()),
    ]);
    let report = runtime_line_attribution_from_files(&files, &package_ownership);

    assert_eq!(report.total_runtime_lines, 8);
    assert_eq!(report.unattributed_lines, 0);
    assert_eq!(report.by_kind["import"].lines, 1);
    assert_eq!(report.by_kind["lazy_value"].lines, 1);
    assert_eq!(report.by_kind["setter"].lines, 1);
    assert_eq!(report.by_kind["function"].lines, 3);
    assert_eq!(report.by_kind["class"].lines, 1);
    assert_eq!(report.by_kind["export"].lines, 1);
    assert_eq!(report.by_package["fixture@1.0.0"].lines, 2);
    assert_eq!(report.by_package["<application>"].lines, 3);
    assert_eq!(report.by_package["ui@2.0.0"].lines, 1);
    assert_eq!(report.by_package["<runtime-glue>"].lines, 2);
    assert!(
        report
            .items
            .iter()
            .any(|item| item.kind == "function" && item.binding == "run" && item.lines == 3),
        "top-level function span should be attributed to run: {:?}",
        report.items
    );
    assert!(
        report
            .items
            .iter()
            .all(|item| item.path.starts_with("modules/runtime/")),
        "only runtime files should be attributed"
    );
}

#[test]
fn runtime_source_span_owner_matches_runtime_wrapper_that_overlaps_module_body() {
    let owners = vec![RuntimeSourceSpanOwner {
        byte_start: 25,
        byte_end: 125,
        label: "zod@3.24.2".to_string(),
    }];

    assert_eq!(
        runtime_source_span_owner_label_for_range(&owners, 10, 150).as_deref(),
        Some("zod@3.24.2")
    );
    assert_eq!(
        runtime_source_span_owner_label_for_range(&owners, 0, 10).as_deref(),
        None
    );
}

#[test]
fn runtime_source_span_owner_reports_shared_cross_package_overlap() {
    let owners = vec![
        RuntimeSourceSpanOwner {
            byte_start: 25,
            byte_end: 75,
            label: "alpha@1.0.0".to_string(),
        },
        RuntimeSourceSpanOwner {
            byte_start: 80,
            byte_end: 125,
            label: "beta@2.0.0".to_string(),
        },
    ];

    assert_eq!(
        runtime_source_span_owner_label_for_range(&owners, 10, 150).as_deref(),
        Some("<shared>")
    );
}

#[test]
fn runtime_module_owner_label_prefers_package_hint_on_application_modules() {
    let mut module = ModuleInput::application(ModuleId(7), "lazy", "lazy").with_source_file(1);
    module.package_name = Some("zod".to_string());
    module.package_version = Some("3.24.2".to_string());

    assert_eq!(runtime_module_owner_label(&module), "zod@3.24.2");
}

#[test]
fn runtime_original_name_owner_labels_runtime_wrapper_by_module_name() {
    let mut module =
        ModuleInput::application(ModuleId(7), "kP7", "modules/7-kp7.ts").with_source_file(1);
    module.package_name = Some("zod".to_string());
    module.package_version = Some("3.24.2".to_string());

    let owners = runtime_original_name_owners_by_binding(&[module]);

    assert_eq!(
        owners
            .get(&(1, "kP7".to_string()))
            .and_then(|labels| labels.iter().next())
            .map(String::as_str),
        Some("zod@3.24.2")
    );
}

#[test]
fn runtime_inventory_selects_project_source_sizes_with_limit_ordering() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let database_path = tempdir.path().join("input.db");
    let connection = Connection::open(database_path.as_path()).expect("open sqlite");
    connection
        .execute_batch(
            r"
            CREATE TABLE projects (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            CREATE TABLE source_files (
                id INTEGER PRIMARY KEY,
                file_path TEXT NOT NULL,
                file_size INTEGER NOT NULL
            );
            CREATE TABLE project_files (project_id INTEGER NOT NULL, file_id INTEGER NOT NULL);

            INSERT INTO projects (id, name) VALUES (1, 'old'), (2, 'middle'), (3, 'new');
            INSERT INTO source_files (id, file_path, file_size)
                VALUES (10, 'one.js', 100), (11, 'two.js', 25), (12, 'three.js', 7);
            INSERT INTO project_files (project_id, file_id)
                VALUES (1, 10), (1, 11), (2, 12);
            ",
        )
        .expect("schema");

    let newest_args = RuntimeInventoryArgs {
        input: database_path.clone(),
        project_id: None,
        all_projects: true,
        limit: Some(2),
        newest: true,
        max_source_bytes: Some(10),
        setter_blockers: false,
        runtime_attribution: false,
        package_source_blockers: false,
    };
    let selections =
        runtime_inventory_project_selections(&newest_args).expect("select newest projects");

    assert_eq!(selections.len(), 2);
    assert_eq!(selections[0].project_id, 3);
    assert_eq!(selections[0].source_bytes, 0);
    assert_eq!(selections[1].project_id, 2);
    assert_eq!(selections[1].source_bytes, 7);

    let single_project_args = RuntimeInventoryArgs {
        input: database_path,
        project_id: Some(1),
        all_projects: false,
        limit: None,
        newest: false,
        max_source_bytes: None,
        setter_blockers: false,
        runtime_attribution: false,
        package_source_blockers: false,
    };
    let selections =
        runtime_inventory_project_selections(&single_project_args).expect("select single project");

    assert_eq!(selections.len(), 1);
    assert_eq!(selections[0].project_id, 1);
    assert_eq!(selections[0].source_bytes, 125);
}

#[test]
fn materialization_hints_resolve_non_exact_versions_from_project_and_cache() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "lodash",
        "node_modules/lodash/index.js",
        "lodash",
        Some("4.17.21".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(11),
        "rxjs",
        "node_modules/rxjs/index.js",
        "rxjs",
        Some("7.x".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(12),
        "zod",
        "node_modules/zod/index.js",
        "zod",
        Some("4.0.0".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(13),
        "react",
        "node_modules/react/index.js",
        "react",
        Some("latest".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(14),
        "protobufjs",
        "node_modules/protobufjs/index.js",
        "protobufjs",
        Some("7.x".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(15),
        "protobufjsExact",
        "node_modules/protobufjs/light.js",
        "protobufjs",
        Some("7.5.4".to_string()),
    ));
    let available_sources = [
        PackageSource::source_only("rxjs", "7.8.2", "rxjs", "rxjs@7.8.2/index.js", "export {};"),
        PackageSource::source_only(
            "protobufjs",
            "7.4.0",
            "protobufjs",
            "protobufjs@7.4.0/index.js",
            "export {};",
        ),
    ];

    let hints = package_version_hints_for_materialization(
        &rows,
        &BTreeSet::from([
            "lodash".to_string(),
            "rxjs".to_string(),
            "react".to_string(),
            "protobufjs".to_string(),
        ]),
        &available_sources,
    );

    assert_eq!(
        hints,
        BTreeSet::from([
            ("lodash".to_string(), "4.17.21".to_string()),
            ("protobufjs".to_string(), "7.5.4".to_string()),
        ])
    );
}

#[test]
fn non_exact_package_versions_resolve_to_best_cached_version_before_matching() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "forgeRange",
        "node_modules/node-forge/index.js",
        "node-forge",
        Some("1.x".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(11),
        "forgeExact",
        "node_modules/node-forge/lib/aes.js",
        "node-forge",
        Some("1.3.1".to_string()),
    ));
    let package_sources = [
        PackageSource::source_only(
            "node-forge",
            "1.0.0",
            "node-forge",
            "node-forge@1.0.0/index.js",
            "export {};",
        ),
        PackageSource::source_only(
            "node-forge",
            "1.3.1",
            "node-forge",
            "node-forge@1.3.1/index.js",
            "export {};",
        ),
        PackageSource::source_only(
            "node-forge",
            "1.3.3",
            "node-forge",
            "node-forge@1.3.3/index.js",
            "export {};",
        ),
    ];

    let resolved = resolve_package_version_hints_to_available_sources(
        &mut rows,
        &package_sources,
        &BTreeSet::new(),
    )
    .expect("resolve package version hints");

    assert_eq!(resolved, 1);
    assert_eq!(rows.modules[0].package_version.as_deref(), Some("1.3.1"));
}

#[test]
fn package_version_resolution_rejects_invalid_package_source_version() {
    let rows = InputRows::new(ProjectInput::new(1, "fixture"));
    let package_sources = [PackageSource::source_only(
        "pkg",
        "not-semver",
        "pkg",
        "pkg@not-semver/index.js",
        "export {};",
    )];

    let error = PackageVersionResolutionPlan::build(&rows, &BTreeSet::new(), &package_sources)
        .expect_err("invalid package source version should fail");

    assert!(matches!(
        error,
        MatchPackagesError::InvalidPackageSourceVersion { .. }
    ));
}

#[test]
fn package_version_resolution_rejects_unparseable_package_source() {
    let rows = InputRows::new(ProjectInput::new(1, "fixture"));
    let package_sources = [PackageSource::source_only(
        "pkg",
        "1.0.0",
        "pkg",
        "pkg@1.0.0/index.js",
        "const =",
    )];

    let error = PackageVersionResolutionPlan::build(&rows, &BTreeSet::new(), &package_sources)
        .expect_err("unparseable package source should fail");

    assert!(matches!(
        error,
        MatchPackagesError::NormalizePackageSource { .. }
    ));
}

#[test]
fn impossible_non_exact_versions_do_not_resolve_to_project_exact_cached_version() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "otelImpossibleRange",
        "node_modules/@opentelemetry/otlp-exporter-base/index.js",
        "@opentelemetry/otlp-exporter-base",
        Some("1.x".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(11),
        "otelExactOlder",
        "node_modules/@opentelemetry/otlp-exporter-base/old.js",
        "@opentelemetry/otlp-exporter-base",
        Some("0.208.0".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(12),
        "otelExactCurrent",
        "node_modules/@opentelemetry/otlp-exporter-base/current.js",
        "@opentelemetry/otlp-exporter-base",
        Some("0.211.0".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(13),
        "otelExactCurrentAgain",
        "node_modules/@opentelemetry/otlp-exporter-base/current-again.js",
        "@opentelemetry/otlp-exporter-base",
        Some("0.211.0".to_string()),
    ));
    let package_sources = [
        PackageSource::source_only(
            "@opentelemetry/otlp-exporter-base",
            "0.208.0",
            "@opentelemetry/otlp-exporter-base",
            "@opentelemetry/otlp-exporter-base@0.208.0/index.js",
            "export {};",
        ),
        PackageSource::source_only(
            "@opentelemetry/otlp-exporter-base",
            "0.211.0",
            "@opentelemetry/otlp-exporter-base",
            "@opentelemetry/otlp-exporter-base@0.211.0/index.js",
            "export {};",
        ),
    ];
    let resolved = resolve_package_version_hints_to_available_sources(
        &mut rows,
        &package_sources,
        &BTreeSet::new(),
    )
    .expect("resolve package version hints");

    assert_eq!(resolved, 0);
    assert_eq!(rows.modules[0].package_version.as_deref(), Some("1.x"));
}

#[test]
fn package_version_resolution_evidence_records_matching_range_resolution() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "pkgRange",
        "node_modules/pkg/index.js",
        "pkg",
        Some("1.x".to_string()),
    ));
    let before = package_versions_by_module(&rows);
    rows.modules[0].package_version = Some("1.2.3".to_string());

    let evidence = package_version_resolution_evidence(&before, &rows);
    let evidence = evidence
        .get(&ModuleId(10))
        .expect("resolution should be recorded");

    assert_eq!(evidence.requested_version.as_deref(), Some("1.x"));
    assert_eq!(evidence.resolved_version.as_str(), "1.2.3");
    assert_eq!(evidence.reason, "range_resolved_to_available_source");
}

#[test]
fn impossible_non_exact_versions_do_not_resolve_when_project_exact_versions_exist() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "otelImpossibleRange",
        "node_modules/@opentelemetry/otlp-exporter-base/index.js",
        "@opentelemetry/otlp-exporter-base",
        Some("1.x".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(11),
        "otelExactOne",
        "node_modules/@opentelemetry/otlp-exporter-base/one.js",
        "@opentelemetry/otlp-exporter-base",
        Some("0.208.0".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(12),
        "otelExactTwo",
        "node_modules/@opentelemetry/otlp-exporter-base/two.js",
        "@opentelemetry/otlp-exporter-base",
        Some("0.211.0".to_string()),
    ));
    let package_sources = [
        PackageSource::source_only(
            "@opentelemetry/otlp-exporter-base",
            "0.208.0",
            "@opentelemetry/otlp-exporter-base",
            "@opentelemetry/otlp-exporter-base@0.208.0/index.js",
            "export {};",
        ),
        PackageSource::source_only(
            "@opentelemetry/otlp-exporter-base",
            "0.211.0",
            "@opentelemetry/otlp-exporter-base",
            "@opentelemetry/otlp-exporter-base@0.211.0/index.js",
            "export {};",
        ),
    ];
    assert_eq!(
        best_matching_package_version_by_binary_search(
            "1.x",
            &BTreeSet::from([
                semver::Version::parse("0.208.0").expect("fixture version should parse"),
                semver::Version::parse("0.211.0").expect("fixture version should parse"),
            ]),
        ),
        None
    );
    let resolved = resolve_package_version_hints_to_available_sources(
        &mut rows,
        &package_sources,
        &BTreeSet::new(),
    )
    .expect("resolve package version hints");

    assert_eq!(resolved, 0);
    assert_eq!(rows.modules[0].package_version.as_deref(), Some("1.x"));
}

#[test]
fn unavailable_exact_package_versions_remain_unchanged_when_exact_source_missing() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "awsMissingExact",
        "node_modules/@aws-sdk/middleware-host-header/index.js",
        "@aws-sdk/middleware-host-header",
        Some("3.712.0".to_string()),
    ));
    let package_sources = [
        PackageSource::source_only(
            "@aws-sdk/middleware-host-header",
            "3.700.0",
            "@aws-sdk/middleware-host-header",
            "@aws-sdk/middleware-host-header@3.700.0/index.js",
            "export {};",
        ),
        PackageSource::source_only(
            "@aws-sdk/middleware-host-header",
            "3.711.0",
            "@aws-sdk/middleware-host-header",
            "@aws-sdk/middleware-host-header@3.711.0/index.js",
            "export {};",
        ),
        PackageSource::source_only(
            "@aws-sdk/middleware-host-header",
            "3.720.0",
            "@aws-sdk/middleware-host-header",
            "@aws-sdk/middleware-host-header@3.720.0/index.js",
            "export {};",
        ),
    ];

    let resolved = resolve_package_version_hints_to_available_sources(
        &mut rows,
        &package_sources,
        &BTreeSet::new(),
    )
    .expect("resolve package version hints");

    assert_eq!(resolved, 0);
    assert_eq!(rows.modules[0].package_version.as_deref(), Some("3.712.0"));
}

#[test]
fn unavailable_exact_package_versions_do_not_rewrite_from_source_identity() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    let bundled_source = "export function add(a,b){return a+b}";
    rows.source_files.push(SourceFileInput::new(
        1,
        "bundle.js",
        Some(bundled_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(10),
            "pkgMissingExact",
            "node_modules/pkg/add.js",
            "pkg",
            Some("3.712.0".to_string()),
        )
        .with_source_file(1),
    );
    let package_sources = [
        PackageSource::source_only(
            "pkg",
            "3.711.0",
            "pkg/add.js",
            "pkg@3.711.0/add.js",
            "export function sub(a,b){return a-b}",
        ),
        PackageSource::source_only(
            "pkg",
            "3.720.0",
            "pkg/add.js",
            "pkg@3.720.0/add.js",
            "export function add(a, b) {\n  return a + b;\n}",
        ),
    ];

    let resolved = resolve_package_version_hints_to_available_sources(
        &mut rows,
        &package_sources,
        &BTreeSet::new(),
    )
    .expect("resolve package version hints");

    assert_eq!(resolved, 0);
    assert_eq!(rows.modules[0].package_version.as_deref(), Some("3.712.0"));
}

#[test]
fn missing_package_versions_prefer_source_identity_over_latest_cached_version() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    let bundled_source = "export function add(a,b){return a+b}";
    rows.source_files.push(SourceFileInput::new(
        1,
        "bundle.js",
        Some(bundled_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(10),
            "pkgNoVersion",
            "node_modules/pkg/add.js",
            "pkg",
            None,
        )
        .with_source_file(1),
    );
    let package_sources = [
        PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg/add.js",
            "pkg@1.0.0/add.js",
            "export function add(a, b) {\n  return a + b;\n}",
        ),
        PackageSource::source_only(
            "pkg",
            "2.0.0",
            "pkg/add.js",
            "pkg@2.0.0/add.js",
            "export function sub(a,b){return a-b}",
        ),
    ];

    let resolved = resolve_package_version_hints_to_available_sources(
        &mut rows,
        &package_sources,
        &BTreeSet::new(),
    )
    .expect("resolve package version hints");

    assert_eq!(resolved, 1);
    assert_eq!(rows.modules[0].package_version.as_deref(), Some("1.0.0"));
}

#[test]
fn package_sources_filter_to_versions_referenced_after_resolution() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "pkgOne",
        "node_modules/pkg/index.js",
        "pkg",
        Some("1.0.0".to_string()),
    ));
    let mut package_sources = vec![
        PackageSource::source_only("pkg", "1.0.0", "pkg", "pkg@1.0.0/index.js", "export {};"),
        PackageSource::source_only("pkg", "2.0.0", "pkg", "pkg@2.0.0/index.js", "export {};"),
        PackageSource::source_only(
            "import-only",
            "3.0.0",
            "import-only",
            "import-only@3.0.0/index.js",
            "export {};",
        ),
    ];

    let removed =
        super::filter_package_sources_to_referenced_package_versions(&rows, &mut package_sources);

    assert_eq!(removed, 1);
    assert_eq!(
        package_sources
            .iter()
            .map(|source| {
                (
                    source.package_name.as_str(),
                    source.package_version.as_str(),
                )
            })
            .collect::<Vec<_>>(),
        vec![("pkg", "1.0.0"), ("import-only", "3.0.0")]
    );
}

#[test]
fn missing_package_versions_resolve_to_latest_cached_version() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "inkNoVersion",
        "node_modules/ink/index.js",
        "ink",
        None,
    ));
    let package_sources = [
        PackageSource::source_only("ink", "4.4.1", "ink", "ink@4.4.1/index.js", "export {};"),
        PackageSource::source_only("ink", "5.2.1", "ink", "ink@5.2.1/index.js", "export {};"),
    ];

    let resolved = resolve_package_version_hints_to_available_sources(
        &mut rows,
        &package_sources,
        &BTreeSet::new(),
    )
    .expect("resolve package version hints");

    assert_eq!(resolved, 1);
    assert_eq!(rows.modules[0].package_version.as_deref(), Some("5.2.1"));
}

#[test]
fn best_matching_package_version_uses_binary_search_for_wildcards() {
    let versions = BTreeSet::from([
        semver::Version::parse("0.9.9").expect("fixture version should parse"),
        semver::Version::parse("1.0.0").expect("fixture version should parse"),
        semver::Version::parse("1.2.3").expect("fixture version should parse"),
        semver::Version::parse("1.9.9").expect("fixture version should parse"),
        semver::Version::parse("2.0.0").expect("fixture version should parse"),
    ]);

    let selected = best_matching_package_version_by_binary_search("1.x", &versions);

    assert_eq!(
        selected.as_ref().map(ToString::to_string).as_deref(),
        Some("1.9.9")
    );
}

#[test]
fn network_version_resolution_requires_exact_match_for_exact_requests() {
    let versions = BTreeSet::from([
        semver::Version::parse("3.354.0").expect("fixture version should parse"),
        semver::Version::parse("3.370.0").expect("fixture version should parse"),
        semver::Version::parse("3.374.0").expect("fixture version should parse"),
    ]);

    let selected = super::resolve_package_version_hint_from_versions("3.712.0", &versions);

    assert_eq!(selected, None);
}

#[test]
fn network_resolution_hints_include_missing_versions() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "inkNoVersion",
        "node_modules/ink/index.js",
        "ink",
        None,
    ));

    let hints = network_package_version_resolution_hints(&rows, &BTreeSet::new(), &[]);

    assert_eq!(
        hints,
        BTreeSet::from([("ink".to_string(), "latest".to_string())])
    );
}

#[test]
fn requested_package_scope_expands_to_dependency_graph_component() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    for (module_id, package_name) in [(10, "alpha"), (11, "beta"), (12, "gamma"), (13, "delta")] {
        rows.modules.push(ModuleInput::package(
            ModuleId(module_id),
            package_name,
            format!("node_modules/{package_name}/index.js"),
            package_name,
            Some("1.0.0".to_string()),
        ));
    }
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(10),
        target: ModuleDependencyTarget::Module(ModuleId(11)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(12),
        target: ModuleDependencyTarget::Module(ModuleId(11)),
    });

    let scope = package_graph_component_scope(&rows, &["alpha".to_string()]);

    assert_eq!(
        scope,
        BTreeSet::from(["alpha".to_string(), "beta".to_string(), "gamma".to_string(),]),
        "requested package matching must see the whole package dependency component, including reverse consumers"
    );
}

#[test]
fn revalidation_removes_external_attributions_for_expanded_component() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.package_attributions
        .push(PackageAttributionInput::accepted_external(
            ModuleId(10),
            "alpha",
            "1.0.0",
            "alpha",
        ));
    rows.package_attributions
        .push(PackageAttributionInput::accepted_external(
            ModuleId(11),
            "beta",
            "1.0.0",
            "beta",
        ));
    rows.package_attributions
        .push(PackageAttributionInput::accepted_external(
            ModuleId(12),
            "delta",
            "1.0.0",
            "delta",
        ));

    let removed = remove_package_attributions_for_revalidation(
        &mut rows,
        &BTreeSet::from(["alpha".to_string(), "beta".to_string()]),
    );

    assert_eq!(removed, 2);
    assert_eq!(rows.package_attributions.len(), 1);
    assert_eq!(
        rows.package_attributions[0].package_name.as_str(),
        "delta",
        "external imports outside the expanded package component stay as existing proof"
    );
}

#[test]
fn package_source_cache_persists_external_importability() {
    let mut connection = Connection::open_in_memory().expect("open sqlite");
    connection
        .execute_batch(
            r"
            CREATE TABLE package_source_cache (
                package_name TEXT NOT NULL,
                package_version TEXT NOT NULL,
                entry_path TEXT NOT NULL,
                source_content TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                fetched_at TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                PRIMARY KEY (package_name, package_version)
            );
            ",
        )
        .expect("create legacy package source cache");
    let sources = vec![
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/public",
            "pkg@1.2.3/public.js",
            "export const publicValue = 1;",
        ),
        PackageSource::source_only(
            "pkg",
            "1.2.3",
            "pkg/internal",
            "pkg@1.2.3/internal.js",
            "export const internalValue = 2;",
        ),
    ];

    let written = persist_package_source_cache(&mut connection, &sources).expect("persist cache");

    assert_eq!(written, 2);
    let rows = InputRows::new(ProjectInput::new(1, "fixture"));
    let loaded = load_package_sources(
        &mut connection,
        &rows,
        &BTreeSet::from(["pkg".to_string()]),
        &[],
        false,
        false,
    )
    .expect("load cache");
    assert_eq!(loaded.len(), 2);
    assert!(
        loaded
            .iter()
            .any(|source| source.source_path.ends_with("public.js")
                && source.export_specifier == "pkg/public"
                && source.external_importable)
    );
    assert!(
        loaded.iter().any(
            |source| source.source_path.ends_with("internal.js") && !source.external_importable
        )
    );
}

#[test]
fn load_package_sources_skips_cache_when_no_package_scope_exists() {
    let mut connection = Connection::open_in_memory().expect("open sqlite");
    connection
        .execute_batch(
            r"
            CREATE TABLE package_source_cache (
                package_name TEXT NOT NULL,
                package_version TEXT NOT NULL,
                entry_path TEXT NOT NULL,
                source_content TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                fetched_at TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                PRIMARY KEY (package_name, package_version, entry_path)
            );
            INSERT INTO package_source_cache
                (package_name, package_version, entry_path, source_content, content_hash, fetched_at, expires_at)
            VALUES
                ('huge', '1.0.0', 'index.js', 'export const value = 1;', 'h', 'now', 'later');
            ",
        )
        .expect("create package source cache");
    let rows = InputRows::new(ProjectInput::new(1, "fixture"));

    let loaded = load_package_sources(&mut connection, &rows, &BTreeSet::new(), &[], false, false)
        .expect("empty package scope should be accepted");

    assert!(loaded.is_empty());
}

#[test]
fn package_source_cache_without_import_policy_version_is_source_only() {
    let mut connection = Connection::open_in_memory().expect("open sqlite");
    connection
        .execute_batch(
            r"
            CREATE TABLE package_source_cache (
                package_name TEXT NOT NULL,
                package_version TEXT NOT NULL,
                entry_path TEXT NOT NULL,
                source_content TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                external_importable INTEGER NOT NULL DEFAULT 1,
                fetched_at TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                PRIMARY KEY (package_name, package_version, entry_path)
            );
            INSERT INTO package_source_cache
                (package_name, package_version, entry_path, source_content,
                 content_hash, external_importable, fetched_at, expires_at)
            VALUES
                ('pkg', '1.2.3', 'public.js', 'export const publicValue = 1;',
                 'hash', 1, 'now', 'later');
            ",
        )
        .expect("create stale package source cache");
    let rows = InputRows::new(ProjectInput::new(1, "fixture"));

    let loaded = load_package_sources(
        &mut connection,
        &rows,
        &BTreeSet::from(["pkg".to_string()]),
        &[],
        false,
        false,
    )
    .expect("load cache");

    assert_eq!(loaded.len(), 1);
    assert!(
        !loaded[0].external_importable,
        "stale cache rows must be revalidated/materialized before external import emission"
    );
}

#[test]
fn externalization_hints_promote_source_only_cache_rows_when_proof_matches() {
    let connection = Connection::open_in_memory().expect("open sqlite");
    connection
        .execute_batch(
            r"
            CREATE TABLE package_externalization_hints (
                package_name TEXT NOT NULL,
                package_version TEXT NOT NULL,
                entry_path TEXT NOT NULL,
                export_specifier TEXT NOT NULL,
                content_hash TEXT,
                normalized_source_hash TEXT,
                public_members_json TEXT,
                proof_policy_version INTEGER NOT NULL
            );
            ",
        )
        .expect("create hints table");
    let source = "exports.Widget = function Widget(){ return 1; };";
    let content_hash = stable_hash(source.as_bytes());
    connection
        .execute(
            r#"
            INSERT INTO package_externalization_hints
                (package_name, package_version, entry_path, export_specifier,
                 content_hash, public_members_json, proof_policy_version)
            VALUES ('pkg', '1.2.3', 'dist/index.cjs', 'pkg',
                    ?1, '["Widget"]', 1)
            "#,
            [content_hash],
        )
        .expect("insert hint");
    let mut sources = vec![PackageSource::source_only(
        "pkg",
        "1.2.3",
        "pkg",
        "pkg@1.2.3/dist/index.cjs",
        source,
    )];

    let promoted = promote_package_sources_with_externalization_hints(
        &connection,
        &BTreeSet::from(["pkg".to_string()]),
        &mut sources,
    )
    .expect("promote hints");

    assert_eq!(promoted, 1);
    assert!(sources.iter().any(|source| source.external_importable));
}

#[test]
fn package_externalization_hints_command_persists_verified_cache_rows() {
    let mut connection = Connection::open_in_memory().expect("open sqlite");
    let source = "export const Widget = 1;";
    let content_hash = stable_hash(source.as_bytes());
    connection
        .execute_batch(
            r"
            CREATE TABLE package_source_cache (
                package_name TEXT NOT NULL,
                package_version TEXT NOT NULL,
                entry_path TEXT NOT NULL,
                source_content TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                external_importable INTEGER NOT NULL DEFAULT 1,
                external_import_policy_version INTEGER NOT NULL DEFAULT 0,
                export_specifier TEXT NOT NULL DEFAULT '',
                fetched_at TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                PRIMARY KEY (package_name, package_version, entry_path)
            );
            ",
        )
        .expect("create cache table");
    connection
        .execute(
            r"
            INSERT INTO package_source_cache
                (package_name, package_version, entry_path, source_content,
                 content_hash, external_importable, external_import_policy_version,
                 export_specifier, fetched_at, expires_at)
            VALUES ('pkg', '1.2.3', 'dist/index.js', ?1, ?2, 1, ?3,
                    'pkg/dist/index.js', 'now', 'later')
            ",
            params![
                source,
                content_hash.as_str(),
                PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION,
            ],
        )
        .expect("insert cache row");

    let outcome = package_externalization_hints_from_connection(
        &mut connection,
        &PackageExternalizationHintsArgs {
            input: PathBuf::from("input.db"),
            package_names: vec!["pkg".to_string()],
            limit: None,
            apply: true,
        },
    )
    .expect("write hints");

    assert_eq!(outcome.scanned_rows, 1);
    assert_eq!(outcome.verified_rows, 1);
    assert_eq!(outcome.written_rows, 1);
    let (stored_hash, normalized_hash, members, policy): (String, String, String, i64) = connection
        .query_row(
            r"
            SELECT content_hash, normalized_source_hash, public_members_json,
                   proof_policy_version
              FROM package_externalization_hints
             WHERE package_name = 'pkg'
               AND package_version = '1.2.3'
               AND entry_path = 'dist/index.js'
            ",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("hint row");
    assert_eq!(stored_hash, content_hash);
    assert!(!normalized_hash.is_empty());
    assert!(members.contains("Widget"));
    assert_eq!(policy, 1);
}

#[test]
fn package_externalization_hints_command_persists_public_export_proofs() {
    let mut connection = Connection::open_in_memory().expect("open sqlite");
    connection
        .execute_batch(
            r"
            CREATE TABLE package_source_cache (
                package_name TEXT NOT NULL,
                package_version TEXT NOT NULL,
                entry_path TEXT NOT NULL,
                source_content TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                external_importable INTEGER NOT NULL DEFAULT 1,
                external_import_policy_version INTEGER NOT NULL DEFAULT 0,
                export_specifier TEXT NOT NULL DEFAULT '',
                fetched_at TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                PRIMARY KEY (package_name, package_version, entry_path)
            );
            ",
        )
        .expect("create cache table");
    let public_source = "export { Widget } from './internal/widget.js';";
    let private_source = "export class Widget { method(){ return 1; } }";
    for (entry_path, source, external_importable, export_specifier) in [
        ("dist/index.js", public_source, 1_i64, "pkg"),
        (
            "dist/internal/widget.js",
            private_source,
            0_i64,
            "pkg/dist/internal/widget.js",
        ),
    ] {
        connection
            .execute(
                r"
                INSERT INTO package_source_cache
                    (package_name, package_version, entry_path, source_content,
                     content_hash, external_importable, external_import_policy_version,
                     export_specifier, fetched_at, expires_at)
                VALUES ('pkg', '1.2.3', ?1, ?2, ?3, ?4, ?5, ?6, 'now', 'later')
                ",
                params![
                    entry_path,
                    source,
                    stable_hash(source.as_bytes()),
                    external_importable,
                    PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION,
                    export_specifier,
                ],
            )
            .expect("insert cache row");
    }

    let outcome = package_externalization_hints_from_connection(
        &mut connection,
        &PackageExternalizationHintsArgs {
            input: PathBuf::from("input.db"),
            package_names: vec!["pkg".to_string()],
            limit: None,
            apply: true,
        },
    )
    .expect("write hints");

    assert_eq!(outcome.scanned_rows, 2);
    assert_eq!(outcome.verified_rows, 2);
    let (export_specifier, members): (String, String) = connection
        .query_row(
            r"
            SELECT export_specifier, public_members_json
              FROM package_externalization_hints
             WHERE package_name = 'pkg'
               AND package_version = '1.2.3'
               AND entry_path = 'dist/internal/widget.js'
            ",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("public proof hint row");
    assert_eq!(export_specifier, "pkg");
    assert!(members.contains("Widget"));

    let mut sources = vec![PackageSource::source_only(
        "pkg",
        "1.2.3",
        "pkg/dist/internal/widget.js",
        "pkg@1.2.3/dist/internal/widget.js",
        private_source,
    )];
    let promoted = promote_package_sources_with_externalization_hints(
        &connection,
        &BTreeSet::from(["pkg".to_string()]),
        &mut sources,
    )
    .expect("promote public proof hint");
    assert_eq!(promoted, 1);
    assert!(
        sources
            .iter()
            .any(|source| source.external_importable && source.export_specifier == "pkg")
    );
}

#[test]
fn source_eliminated_metric_counts_externalized_private_closure() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "root",
        "pkg/root.ts",
        "pkg",
        Some("1.0.0".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(11),
        "private",
        "pkg/private.ts",
        "pkg",
        Some("1.0.0".to_string()),
    ));
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(10),
        target: ModuleDependencyTarget::Module(ModuleId(11)),
    });
    let mut private_ownership = PackageAttributionInput::rejected_source(
        ModuleId(11),
        "pkg",
        "matched package ownership, but the evidence does not prove a safe single external import",
    );
    private_ownership.package_version = Some("1.0.0".to_string());
    rows.package_attributions.push(private_ownership);
    let report = VersionedPackageMatchReport {
        attributions: vec![PackageAttributionInput::accepted_external(
            ModuleId(10),
            "pkg",
            "1.0.0",
            "pkg",
        )],
        surfaces: Vec::new(),
        matches: Vec::new(),
        version_matches: Vec::new(),
        audit: AuditReport::default(),
    };

    assert_eq!(
        source_eliminated_package_modules_for_report(&rows, &report),
        2
    );
    let stats = package_source_elimination_stats_for_report(&rows, &report, 2);
    assert_eq!(stats.direct_external_import_modules, 1);
    assert_eq!(stats.private_source_suppressed_package_modules, 1);
    assert_eq!(stats.source_eliminated_package_modules, 2);
    assert_eq!(stats.remaining_package_source_modules, 0);
}

#[test]
fn materialize_mode_uses_only_current_policy_cache_rows_as_match_sources() {
    let mut connection = Connection::open_in_memory().expect("open sqlite");
    connection
        .execute_batch(
            r"
            CREATE TABLE package_source_cache (
                package_name TEXT NOT NULL,
                package_version TEXT NOT NULL,
                entry_path TEXT NOT NULL,
                source_content TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                external_importable INTEGER NOT NULL DEFAULT 1,
                external_import_policy_version INTEGER NOT NULL DEFAULT 0,
                export_specifier TEXT NOT NULL DEFAULT '',
                fetched_at TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                PRIMARY KEY (package_name, package_version, entry_path)
            );
            INSERT INTO package_source_cache
                (package_name, package_version, entry_path, source_content,
                 content_hash, external_importable, external_import_policy_version,
                 export_specifier, fetched_at, expires_at)
            VALUES
                ('pkg', '1.x', 'internal.js', 'export const stale = 1;',
                 'stale', 1, 0, '', 'now', 'later'),
                ('pkg', '1.2.3', 'public.js', 'export const current = 1;',
                 'current', 1, 4, 'pkg/public', 'now', 'later');
            ",
        )
        .expect("create package source cache");
    let rows = InputRows::new(ProjectInput::new(1, "fixture"));

    let loaded = load_package_sources(
        &mut connection,
        &rows,
        &BTreeSet::from(["pkg".to_string()]),
        &[],
        true,
        false,
    )
    .expect("load cache");

    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].package_version, "1.2.3");
    assert_eq!(loaded[0].export_specifier, "pkg/public");
    assert!(loaded[0].external_importable);
}

#[test]
fn package_source_cache_stale_policy_versions_are_materialization_hints() {
    let connection = Connection::open_in_memory().expect("open sqlite");
    connection
        .execute_batch(
            r"
            CREATE TABLE package_source_cache (
                package_name TEXT NOT NULL,
                package_version TEXT NOT NULL,
                entry_path TEXT NOT NULL,
                source_content TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                external_importable INTEGER NOT NULL DEFAULT 1,
                external_import_policy_version INTEGER NOT NULL DEFAULT 0,
                export_specifier TEXT NOT NULL DEFAULT '',
                fetched_at TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                PRIMARY KEY (package_name, package_version, entry_path)
            );
            INSERT INTO package_source_cache
                (package_name, package_version, entry_path, source_content,
                 content_hash, external_importable, external_import_policy_version, export_specifier,
                 fetched_at, expires_at)
            VALUES
                ('pkg', '1.2.3', 'index.js', 'export const oldValue = 1;',
                 'hash-a', 1, 0, '', 'now', 'later'),
                ('pkg', '1.2.4', 'index.js', 'export const newValue = 1;',
                 'hash-b', 1, 4, 'pkg', 'now', 'later'),
                ('other', '9.9.9', 'index.js', 'export const other = 1;',
                 'hash-c', 1, 0, '', 'now', 'later');
            ",
        )
        .expect("create mixed policy package source cache");

    let stale =
        stale_package_source_cache_versions(&connection, &BTreeSet::from(["pkg".to_string()]))
            .expect("query stale cache versions");

    assert_eq!(
        stale,
        BTreeSet::from([("pkg".to_string(), "1.2.3".to_string())])
    );
}

#[test]
fn stale_cache_materialization_hints_resolve_ranges_to_project_versions() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(1),
        "m1",
        "lodash/add.js",
        "lodash",
        Some("4.2.0".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(2),
        "m2",
        "lodash/map.js",
        "lodash",
        Some("4.x".to_string()),
    ));
    let existing_sources = [
        PackageSource::source_only(
            "lodash",
            "4.2.0",
            "lodash/add",
            "lodash@4.2.0/add.js",
            "export {};",
        ),
        PackageSource::source_only(
            "lodash",
            "4.17.21",
            "lodash/add",
            "lodash@4.17.21/add.js",
            "export {};",
        ),
    ];
    let stale = BTreeSet::from([
        ("lodash".to_string(), "4.x".to_string()),
        ("lodash".to_string(), "4.17.21".to_string()),
    ]);

    let hints = stale_cache_version_hints_for_materialization(
        &rows,
        &BTreeSet::from(["lodash".to_string()]),
        &existing_sources,
        &stale,
    );

    assert_eq!(
        hints,
        BTreeSet::from([("lodash".to_string(), "4.2.0".to_string())]),
        "stale range cache rows must materialize the resolved project version, not raw 4.x or unrelated cached versions"
    );
}

#[test]
fn local_package_source_collection_prefers_compiled_runtime_family_over_src_ts() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let package_dir = tempdir.path().join("node_modules/pkg");
    fs::create_dir_all(package_dir.join("src")).expect("create src dir");
    fs::create_dir_all(package_dir.join("dist")).expect("create dist dir");
    fs::write(
        package_dir.join("package.json"),
        r#"{"name":"pkg","version":"1.2.3","main":"dist/index.js"}"#,
    )
    .expect("write package json");
    fs::write(
        package_dir.join("src/index.ts"),
        "export const tsSource: number = 1;",
    )
    .expect("write src ts");
    fs::write(
        package_dir.join("dist/index.js"),
        "export const jsSource = 1;",
    )
    .expect("write dist js");
    let metadata = local_package_metadata(package_dir.as_path())
        .expect("read metadata")
        .expect("metadata");
    let mut sources = Vec::new();

    collect_local_package_sources(package_dir.as_path(), &metadata, &mut sources)
        .expect("collect sources");

    // package.json is also collected now (read for cache-anchored surface
    // resolution), so locate the importable code source by path.
    let dist = sources
        .iter()
        .find(|source| source.source_path.ends_with("dist/index.js"))
        .expect("dist/index.js source");
    assert!(dist.external_importable);
    assert!(
        !sources
            .iter()
            .any(|source| source.source_path.ends_with("src/index.ts"))
    );
}

#[test]
fn local_package_metadata_rejects_unparseable_package_json() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let package_dir = tempdir.path().join("node_modules/pkg");
    fs::create_dir_all(package_dir.as_path()).expect("create package dir");
    fs::write(package_dir.join("package.json"), r#"{"name":"pkg","#)
        .expect("write invalid package json");

    let error = local_package_metadata(package_dir.as_path())
        .expect_err("invalid package metadata should fail");

    assert!(matches!(
        error,
        MatchPackagesError::InvalidPackageMetadata { .. }
    ));
}

#[test]
fn local_package_source_collection_wraps_importable_json_data() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let package_dir = tempdir.path().join("node_modules/css-color-names");
    fs::create_dir_all(package_dir.as_path()).expect("create package dir");
    fs::write(
        package_dir.join("package.json"),
        r#"{"name":"css-color-names","version":"1.0.1","main":"./css-color-names.json"}"#,
    )
    .expect("write package json");
    fs::write(
        package_dir.join("css-color-names.json"),
        r##"{"aliceblue":"#f0f8ff","rebeccapurple":"#663399"}"##,
    )
    .expect("write json data");
    fs::write(package_dir.join("ignored.json"), r#"{"private":true}"#).expect("write private json");
    let metadata = local_package_metadata(package_dir.as_path())
        .expect("read metadata")
        .expect("metadata");
    let mut sources = Vec::new();

    collect_local_package_sources(package_dir.as_path(), &metadata, &mut sources)
        .expect("collect sources");

    // Two sources: the importable JSON data plus the root package.json, which
    // is now always kept for cache-anchored surface resolution. The unrelated
    // `ignored.json` is still dropped (no importable target).
    assert_eq!(sources.len(), 2);
    assert!(
        !sources
            .iter()
            .any(|source| source.source_path.ends_with("ignored.json"))
    );
    let data = sources
        .iter()
        .find(|source| source.source_path.ends_with("css-color-names.json"))
        .expect("css-color-names.json source");
    assert_eq!(data.package_name, "css-color-names");
    assert_eq!(data.package_version, "1.0.1");
    assert!(data.external_importable);
    assert!(data.source.starts_with("export default "));
    assert!(data.source.contains("aliceblue"));
}

#[test]
fn local_package_source_collection_keeps_exported_package_json() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let package_dir = tempdir.path().join("node_modules/pkg");
    fs::create_dir_all(package_dir.as_path()).expect("create package dir");
    fs::write(
        package_dir.join("package.json"),
        r#"{"name":"pkg","version":"1.2.3","exports":{"./package.json":"./package.json"}}"#,
    )
    .expect("write package json");
    let metadata = local_package_metadata(package_dir.as_path())
        .expect("read metadata")
        .expect("metadata");
    let mut sources = Vec::new();

    collect_local_package_sources(package_dir.as_path(), &metadata, &mut sources)
        .expect("collect sources");

    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].export_specifier, "pkg/package.json");
    assert!(sources[0].source_path.ends_with("package.json"));
    assert!(sources[0].external_importable);
    assert!(sources[0].source.starts_with("export default "));
}

#[test]
fn package_source_build_variant_selection_uses_semantic_path_hints() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "m10",
        "modules/10-rxjs/operators/sample.ts",
        "rxjs",
        Some("7.8.2".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(11),
        "m11",
        "modules/11-rxjs/_internal/is-array-like.ts",
        "rxjs",
        Some("7.8.2".to_string()),
    ));
    let mut sources = vec![
        PackageSource::source_only(
            "rxjs",
            "7.8.2",
            "rxjs/operators/sample",
            "rxjs@7.8.2/dist/cjs/operators/sample.js",
            "exports.sample = sample;",
        ),
        PackageSource::source_only(
            "rxjs",
            "7.8.2",
            "rxjs/internal/isArrayLike",
            "rxjs@7.8.2/dist/cjs/internal/util/isArrayLike.js",
            "exports.isArrayLike = isArrayLike;",
        ),
        PackageSource::source_only(
            "rxjs",
            "7.8.2",
            "rxjs/operators/sample",
            "rxjs@7.8.2/dist/esm/operators/sample.js",
            "export function sample() {}",
        ),
        PackageSource::source_only(
            "rxjs",
            "7.8.2",
            "rxjs/internal/isArrayLike",
            "rxjs@7.8.2/dist/esm/internal/util/isArrayLike.js",
            "export function isArrayLike() {}",
        ),
        PackageSource::source_only(
            "rxjs",
            "7.8.2",
            "rxjs/internal/unrelated",
            "rxjs@7.8.2/src/internal/unrelated.ts",
            "export const unrelated = 1;",
        ),
    ];

    super::filter_package_sources_to_best_build_variants(&rows, &mut sources);

    assert_eq!(sources.len(), 2);
    assert!(
        sources
            .iter()
            .all(|source| source.source_path.contains("/dist/esm/")),
        "{sources:?}"
    );
}

#[test]
fn package_source_build_variant_selection_prefers_full_source_unit_variant_hint() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "m10",
        "rxjs/dist/esm/operators/sample",
        "rxjs",
        Some("7.8.2".to_string()),
    ));
    let mut sources = vec![
        PackageSource::source_only(
            "rxjs",
            "7.8.2",
            "rxjs/operators/sample",
            "rxjs@7.8.2/dist/cjs/operators/sample.js",
            "exports.sample = sample;",
        ),
        PackageSource::source_only(
            "rxjs",
            "7.8.2",
            "rxjs/operators/sample",
            "rxjs@7.8.2/dist/esm/operators/sample.js",
            "export function sample() {}",
        ),
    ];

    super::filter_package_sources_to_best_build_variants(&rows, &mut sources);

    assert_eq!(sources.len(), 1);
    assert!(sources[0].source_path.contains("/dist/esm/"));
}

#[test]
fn package_source_build_variant_selection_keeps_equal_score_importable_family() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "m10",
        "modules/10-rxjs/operators/sample.ts",
        "rxjs",
        Some("7.8.2".to_string()),
    ));
    let mut sources = vec![
        PackageSource::external(
            "rxjs",
            "7.8.2",
            "rxjs/internal/operators/sample",
            "rxjs@7.8.2/dist/cjs/internal/operators/sample.js",
            "exports.sample = sample;",
        ),
        PackageSource::source_only(
            "rxjs",
            "7.8.2",
            "rxjs/dist/esm/internal/operators/sample.js",
            "rxjs@7.8.2/dist/esm/internal/operators/sample.js",
            "export function sample() {}",
        ),
    ];

    super::filter_package_sources_to_best_build_variants(&rows, &mut sources);

    assert_eq!(sources.len(), 2);
    assert!(
        sources
            .iter()
            .any(|source| source.source_path.contains("/dist/esm/") && !source.external_importable),
        "{sources:?}"
    );
    assert!(
        sources
            .iter()
            .any(|source| source.source_path.contains("/dist/cjs/") && source.external_importable),
        "{sources:?}"
    );
}

#[test]
fn package_source_build_variant_selection_scores_export_surface_hints() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "m10",
        "pkg/public/client.js",
        "pkg",
        Some("1.0.0".to_string()),
    ));
    let mut sources = vec![
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/public/client",
            "pkg@1.0.0/dist/index.js",
            "export const client = 1;",
        ),
        PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg/public/client",
            "pkg@1.0.0/src/public/client.ts",
            "export const client = 1;",
        ),
    ];

    filter_package_sources_to_best_build_variants(&rows, &mut sources);

    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].export_specifier, "pkg/public/client");
    assert!(sources[0].source_path.contains("/dist/index.js"));
    assert!(sources[0].external_importable);
}

#[test]
fn package_source_build_variant_selection_keeps_root_export_surface() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "m10",
        "pkg/public/client.js",
        "pkg",
        Some("1.0.0".to_string()),
    ));
    let mut sources = vec![
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg",
            "pkg@1.0.0/index.js",
            "export const root = 1;",
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/public/client",
            "pkg@1.0.0/dist/index.js",
            "export const client = 1;",
        ),
        PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg/public/client",
            "pkg@1.0.0/src/public/client.ts",
            "export const client = 1;",
        ),
    ];

    filter_package_sources_to_best_build_variants(&rows, &mut sources);

    assert!(
        sources.iter().any(|source| source.export_specifier == "pkg"
            && source.source_path.ends_with("index.js")
            && source.external_importable),
        "{sources:?}"
    );
    assert!(
        sources
            .iter()
            .any(|source| source.export_specifier == "pkg/public/client"
                && source.source_path.contains("/dist/index.js")),
        "{sources:?}"
    );
}

#[test]
fn package_source_build_variant_selection_keeps_root_manifest_for_surface() {
    // rxjs ships `"./package.json": "./package.json"` in its exports, so the
    // cached root manifest carries export_specifier `rxjs/package.json` (not the
    // bare package name). The build-variant filter must still keep it: the
    // cache-anchored surface resolver reads the root package.json to learn the
    // package's public API, and the project's hints point at dist/cjs, not root.
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "m10",
        "modules/10-rxjs/internal/replay-subject.ts",
        "rxjs",
        Some("7.8.2".to_string()),
    ));
    let mut sources = vec![
        PackageSource::source_only(
            "rxjs",
            "7.8.2",
            "rxjs/internal/replay-subject",
            "rxjs@7.8.2/dist/cjs/internal/replay-subject.js",
            "exports.ReplaySubject = ReplaySubject;",
        ),
        PackageSource::external(
            "rxjs",
            "7.8.2",
            "rxjs/package.json",
            "rxjs@7.8.2/package.json",
            r#"export default {"name":"rxjs","exports":{".":"./dist/cjs/index.js","./internal/*":"./dist/cjs/internal/*.js","./package.json":"./package.json"}};"#,
        ),
    ];

    filter_package_sources_to_best_build_variants(&rows, &mut sources);

    assert!(
        sources
            .iter()
            .any(|source| source.source_path.ends_with("/package.json")),
        "root manifest must survive build-variant filtering for surface resolution: {sources:?}"
    );
}

#[test]
fn package_source_path_hint_filter_keeps_root_manifest_for_surface() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "m10",
        "modules/10-rxjs/internal/replay-subject.ts",
        "rxjs",
        Some("7.8.2".to_string()),
    ));
    let mut sources = (0..300)
        .map(|index| {
            PackageSource::source_only(
                "rxjs",
                "7.8.2",
                format!("rxjs/internal/private-{index}"),
                format!("rxjs@7.8.2/dist/cjs/internal/private-{index}.js"),
                "exports.privateValue = 1;",
            )
        })
        .collect::<Vec<_>>();
    sources.push(PackageSource::external(
        "rxjs",
        "7.8.2",
        "rxjs/package.json",
        "rxjs@7.8.2/package.json",
        r#"export default {"name":"rxjs","exports":{".":"./dist/cjs/index.js","./package.json":"./package.json"}};"#,
    ));

    filter_package_sources_to_relevant_path_hints(&rows, &mut sources);

    assert!(
        sources
            .iter()
            .any(|source| source.source_path.ends_with("/package.json")),
        "root manifest must survive path-hint filtering for surface resolution: {sources:?}"
    );
}

#[test]
fn package_source_path_hint_filter_keeps_export_surface_match() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "m10",
        "pkg/public/client.js",
        "pkg",
        Some("1.0.0".to_string()),
    ));
    let mut sources = (0..300)
        .map(|index| {
            PackageSource::source_only(
                "pkg",
                "1.0.0",
                format!("pkg/private-{index}"),
                format!("pkg@1.0.0/dist/chunk-{index}.js"),
                "export const privateValue = 1;",
            )
        })
        .collect::<Vec<_>>();
    sources.push(PackageSource::external(
        "pkg",
        "1.0.0",
        "pkg/public/client",
        "pkg@1.0.0/dist/index.js",
        "export const client = 1;",
    ));

    filter_package_sources_to_relevant_path_hints(&rows, &mut sources);

    assert!(
        sources
            .iter()
            .any(|source| source.export_specifier == "pkg/public/client"
                && source.source_path.ends_with("dist/index.js")
                && source.external_importable),
        "{sources:?}"
    );
}

#[test]
fn package_source_path_hint_filter_keeps_body_semantic_member_match() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "m10",
        "modules/10-opentelemetry/api/diag-log-level.ts",
        "@opentelemetry/api",
        Some("1.9.1".to_string()),
    ));
    let mut sources = (0..300)
        .map(|index| {
            PackageSource::source_only(
                "@opentelemetry/api",
                "1.9.1",
                format!("@opentelemetry/api/private-{index}"),
                format!("@opentelemetry/api@1.9.1/build/src/private-{index}.js"),
                "exports.privateValue = 1;",
            )
        })
        .collect::<Vec<_>>();
    sources.push(PackageSource::source_only(
        "@opentelemetry/api",
        "1.9.1",
        "@opentelemetry/api/build/src/diag/types",
        "@opentelemetry/api@1.9.1/build/src/diag/types.js",
        "exports.DiagLogLevel = void 0;",
    ));

    filter_package_sources_to_relevant_path_hints(&rows, &mut sources);

    assert!(
        sources
            .iter()
            .any(|source| source.source_path.ends_with("build/src/diag/types.js")),
        "{sources:?}"
    );
}

#[test]
fn source_unit_path_hints_enrich_package_module_semantic_path() {
    let connection = Connection::open_in_memory().expect("open in-memory database");
    connection
        .execute_batch(
            r"
            CREATE TABLE source_units (
                project_id INTEGER NOT NULL,
                file_id INTEGER,
                logical_path TEXT NOT NULL,
                package_name TEXT,
                package_version TEXT
            );
            INSERT INTO source_units
                (project_id, file_id, logical_path, package_name, package_version)
            VALUES
                (1, 7, 'webpack://app/./node_modules/rxjs/dist/esm/operators/sample.js',
                 'rxjs', '7.8.2');
            ",
        )
        .expect("seed source_units");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(
        ModuleInput::package(ModuleId(10), "m10", "modules/10.ts", "rxjs", None)
            .with_source_file(7),
    );

    super::enrich_package_modules_from_source_units(&connection, &mut rows, 1)
        .expect("enrich from source_units");

    assert_eq!(
        rows.modules[0].semantic_path,
        "rxjs/dist/esm/operators/sample"
    );
    assert_eq!(rows.modules[0].package_version.as_deref(), Some("7.8.2"));
}

#[test]
fn match_package_audit_findings_are_deduplicated() {
    let finding = AuditFinding::error(FindingCode::UnparseablePackageSource, "parse failed")
        .with_module("pkg@1.0.0/src/index.ts")
        .with_binding("pkg@1.0.0");
    let mut audit = AuditReport::default();
    audit.push(finding.clone());
    audit.push(finding);

    let deduped = dedup_audit_report(audit);

    assert_eq!(deduped.findings().len(), 1);
}

#[test]
fn output_paths_cannot_escape_output_directory() {
    let error = checked_output_path(PathBuf::from("out").as_path(), "../escape.ts");

    assert!(error.is_err());
}

#[test]
fn project_writer_emits_typescript_scaffold() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let files = vec![EmittedFile {
        path: "modules/1-entry.ts".to_string(),
        source: "// @ts-nocheck\nconsole.log('ok');".to_string(),
    }];

    let written = write_emitted_project(
        &files,
        &[],
        tempdir.path(),
        &[RuntimeDependency {
            package_name: "undici".to_string(),
            package_version: "2.2.1".to_string(),
        }],
    )
    .expect("project should be written");

    assert_eq!(written, 1);
    assert!(tempdir.path().join("modules/1-entry.ts").exists());
    assert!(
        fs::read_to_string(tempdir.path().join("package.json"))
            .expect("package json")
            .contains("\"check\": \"tsc --noEmit -p tsconfig.json\"")
    );
    assert!(
        fs::read_to_string(tempdir.path().join("package.json"))
            .expect("package json")
            .contains("\"undici\": \"2.2.1\"")
    );
    assert!(
        fs::read_to_string(tempdir.path().join("package.json"))
            .expect("package json")
            .contains("\"@types/node\": \"*\"")
    );
    assert!(
        !tempdir.path().join(".npmrc").exists(),
        "npmrc should only be written for known peer conflicts"
    );
    assert!(
        fs::read_to_string(tempdir.path().join("tsconfig.json"))
            .expect("tsconfig")
            .contains("\"modules/**/*.ts\"")
    );
    assert!(tempdir.path().join("tsconfig.runtime.json").exists());
}

#[test]
fn project_writer_emits_npmrc_for_source_preserved_peer_conflict() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let files = vec![EmittedFile {
        path: "modules/1-entry.ts".to_string(),
        source: "// @ts-nocheck\nconsole.log('ok');".to_string(),
    }];

    write_emitted_project(
        &files,
        &[],
        tempdir.path(),
        &[
            RuntimeDependency {
                package_name: "ink".to_string(),
                package_version: "7.0.3".to_string(),
            },
            RuntimeDependency {
                package_name: "react".to_string(),
                package_version: "19.1.5".to_string(),
            },
            RuntimeDependency {
                package_name: "react-devtools-core".to_string(),
                package_version: "4.28.5".to_string(),
            },
        ],
    )
    .expect("project should be written");

    assert_eq!(
        fs::read_to_string(tempdir.path().join(".npmrc")).expect("npmrc"),
        "legacy-peer-deps=true\n"
    );
}

#[test]
fn project_writer_emits_npmrc_for_externalized_zod_anthropic_peer_conflict() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let files = vec![EmittedFile {
        path: "modules/1-entry.ts".to_string(),
        source: "// @ts-nocheck\nconsole.log('ok');".to_string(),
    }];

    write_emitted_project(
        &files,
        &[],
        tempdir.path(),
        &[
            RuntimeDependency {
                package_name: "@anthropic-ai/sdk".to_string(),
                package_version: "0.91.1".to_string(),
            },
            RuntimeDependency {
                package_name: "zod".to_string(),
                package_version: "3.22.5".to_string(),
            },
        ],
    )
    .expect("project should be written");

    assert_eq!(
        fs::read_to_string(tempdir.path().join(".npmrc")).expect("npmrc"),
        "legacy-peer-deps=true\n"
    );
}

#[test]
fn project_writer_materializes_react_esm_compat_shims() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let files = vec![EmittedFile {
        path: "modules/1-entry.ts".to_string(),
        source: "// @ts-nocheck\nconsole.log('ok');".to_string(),
    }];

    write_emitted_project(
        &files,
        &[],
        tempdir.path(),
        &[
            RuntimeDependency {
                package_name: "react".to_string(),
                package_version: "19.1.5".to_string(),
            },
            RuntimeDependency {
                package_name: "react-dom".to_string(),
                package_version: "18.3.1".to_string(),
            },
        ],
    )
    .expect("project should be written");

    let package_json = fs::read_to_string(tempdir.path().join("package.json"))
        .expect("package json should be written");
    let react_shim = fs::read_to_string(tempdir.path().join("vendor-shims/react/index.js"))
        .expect("react shim should be written");
    let react_dom_shim = fs::read_to_string(tempdir.path().join("vendor-shims/react-dom/index.js"))
        .expect("react-dom shim should be written");

    assert!(package_json.contains("\"react\": \"file:./vendor-shims/react\""));
    assert!(package_json.contains("\"react-cjs\": \"npm:react@19.1.5\""));
    assert!(package_json.contains("\"react-dom\": \"file:./vendor-shims/react-dom\""));
    assert!(package_json.contains("\"react-dom-cjs\": \"npm:react-dom@18.3.1\""));
    assert!(react_shim.contains("export const useEffectEvent"));
    assert!(react_dom_shim.contains("const load = () =>"));
    assert!(
        fs::read_to_string(tempdir.path().join("vendor-shims/react/package.json"))
            .expect("react package")
            .contains("\"version\": \"19.2.0\"")
    );
}

#[test]
fn project_writer_adds_sentry_opentelemetry_peer_dependencies() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let files = vec![EmittedFile {
        path: "modules/1-entry.ts".to_string(),
        source: "// @ts-nocheck\nconsole.log('ok');".to_string(),
    }];

    write_emitted_project(
        &files,
        &[],
        tempdir.path(),
        &[RuntimeDependency {
            package_name: "@sentry/node".to_string(),
            package_version: "8.55.0".to_string(),
        }],
    )
    .expect("project should be written");

    let package_json = fs::read_to_string(tempdir.path().join("package.json"))
        .expect("package json should be written");
    assert!(package_json.contains("\"@opentelemetry/context-async-hooks\": \"^1.30.1\""));
    assert!(package_json.contains("\"@opentelemetry/instrumentation\": \"^0.57.1\""));
}

#[test]
fn project_writer_exposes_cli_entrypoint_when_planned() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let files = vec![EmittedFile {
        path: "cli.ts".to_string(),
        source: "#!/usr/bin/env node\n// @ts-nocheck\nconsole.log('ok');".to_string(),
    }];

    let written =
        write_emitted_project(&files, &[], tempdir.path(), &[]).expect("project should be written");
    let package_json = fs::read_to_string(tempdir.path().join("package.json"))
        .expect("package json should be written");
    let tsconfig = fs::read_to_string(tempdir.path().join("tsconfig.json")).expect("tsconfig");

    assert_eq!(written, 1);
    assert!(tempdir.path().join("cli.ts").exists());
    assert!(package_json.contains("\"start\": \"node ./dist/cli.js\""));
    assert!(package_json.contains("\"reverts-output\": \"./dist/cli.js\""));
    assert!(tsconfig.contains("\"cli.ts\""));
}

#[test]
fn project_writer_materializes_assets_and_build_copy_script() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let files = vec![EmittedFile {
        path: "modules/1-entry.ts".to_string(),
        source: "// @ts-nocheck\nexport const ok = true;".to_string(),
    }];
    let assets = vec![EmittedAsset {
        path: "modules/1-entry/vendor/rg".to_string(),
        bytes: b"rg-binary".to_vec(),
        executable: true,
    }];

    let written = write_emitted_project(&files, &assets, tempdir.path(), &[])
        .expect("project should be written");
    let asset_path = tempdir.path().join("modules/1-entry/vendor/rg");
    let package_json = fs::read_to_string(tempdir.path().join("package.json"))
        .expect("package json should be written");
    let copy_assets = fs::read_to_string(tempdir.path().join("scripts/copy-assets.mjs"))
        .expect("copy-assets script should be written");

    assert_eq!(written, 2);
    assert_eq!(
        fs::read(asset_path.as_path()).expect("asset bytes should be written"),
        b"rg-binary"
    );
    assert!(package_json.contains("node ./scripts/copy-assets.mjs"));
    assert!(copy_assets.contains("modules/1-entry/vendor/rg"));
    assert!(copy_assets.contains("dist/modules/1-entry/vendor/rg"));
    assert!(copy_assets.contains("\"executable\": true"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = fs::metadata(asset_path.as_path())
            .expect("asset metadata")
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0);
    }
}

#[test]
fn match_packages_runs_bundle_extraction_before_matcher() {
    let tempdir = tempfile::tempdir().expect("tempdir");
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
    let mut connection = package_match_connection(bundle_path.clone(), bundle_src, &[]);
    // Replace the default seeded module (id=10, package kind) with an
    // application-kind module that carries no package_name. Bundle
    // extraction will discover the `node_modules/example/index.js` inner
    // module and inject a new package-kind row, bumping
    // loaded_package_modules to >= 1.
    connection
        .execute_batch(
            "DELETE FROM modules WHERE id = 10;
             INSERT INTO modules (id, file_id, original_name, semantic_name, module_category,
                                  package_name, package_version, byte_start, byte_end)
             VALUES (10, 1, 'lib', 'bundle/lib', 'application', NULL, NULL, 0, 0);",
        )
        .expect("seed module");

    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: false,
        package_names: Vec::new(),
        package_source_roots: Vec::new(),
        materialize_package_sources: false,
    };
    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");
    assert!(
        outcome.loaded_package_modules >= 1,
        "extraction should have produced at least one package module: {outcome:?}"
    );
}

#[test]
fn match_packages_skips_cache_and_pipeline_when_no_package_scope_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let mut connection = package_match_connection(
        tempdir.path().join("app.js"),
        "function local(){return 1;}",
        &[(
            "unused",
            "1.0.0",
            "index.js",
            "export function unused(){return 1;}",
        )],
    );
    connection
        .execute_batch(
            "DELETE FROM modules WHERE id = 10;
             INSERT INTO modules (id, file_id, original_name, semantic_name, module_category,
                                  package_name, package_version, byte_start, byte_end)
             VALUES (10, 1, 'app', 'src/app', 'application', NULL, NULL, 0, 0);",
        )
        .expect("seed application module");
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: false,
        package_names: Vec::new(),
        package_source_roots: Vec::new(),
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
    assert_eq!(outcome.loaded_package_modules, 0);
    assert_eq!(outcome.loaded_package_sources, 0);
    assert_eq!(outcome.matched_modules, 0);
    assert_eq!(outcome.function_attributions, 0);
}

#[test]
fn match_packages_dry_run_does_not_write_attribution() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        "export function add(a,b){return a+b}",
        &[(
            "pkg",
            "1.2.3",
            "add.js",
            "export function add(a, b) {\n  return a + b;\n}",
        )],
    );
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: false,
        package_names: Vec::new(),
        package_source_roots: Vec::new(),
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert!(outcome.audit.is_clean());
    assert_eq!(outcome.loaded_package_modules, 1);
    assert_eq!(outcome.package_source_quality_trusted, 1);
    assert_eq!(outcome.package_source_quality_invalid, 0);
    assert_eq!(outcome.loaded_package_sources, 1);
    assert_eq!(outcome.matched_modules, 1);
    assert_eq!(outcome.matched_package_surfaces, 0);
    assert_eq!(outcome.written_attributions, 0);
    assert_eq!(outcome.written_surfaces, 0);
    assert_eq!(package_attribution_count(&connection), 0);
}

#[test]
fn match_packages_revalidates_requested_existing_accepted_attribution() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        "function localOnly(){return 1;}",
        &[(
            "pkg",
            "1.2.3",
            "add.js",
            "export function add(a, b) {\n  return a + b;\n}",
        )],
    );
    connection
        .execute(
            r"
            INSERT INTO package_attributions
                (module_id, module_original_name, package_name, package_version,
                 package_subpath, resolved_file, export_specifier, emission_mode,
                 status, evidence_json, rejection_reason, created_at, updated_at)
            VALUES (10, 'm10', 'pkg', '1.2.3',
                    'add.js', 'pkg@1.2.3/add.js', 'pkg/add.js',
                    'external_import', 'accepted', '{}', NULL, 'old', 'old')
            ",
            [],
        )
        .expect("seed stale accepted attribution");
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: true,
        package_names: vec!["pkg".to_string()],
        package_source_roots: Vec::new(),
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
    assert_eq!(
        package_attribution_count(&connection),
        1,
        "revalidation should overwrite the stale row instead of adding a duplicate"
    );
    let (
        status,
        emission_mode,
        export_specifier,
        external_import_policy_version,
        rejection_reason,
    ): (String, String, Option<String>, i64, Option<String>) = connection
        .query_row(
            r"
            SELECT status, emission_mode, export_specifier,
                   external_import_policy_version, rejection_reason
              FROM package_attributions
             WHERE module_id = 10
            ",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .expect("stale accepted attribution should be rewritten");
    assert_eq!(status, "accepted");
    assert_eq!(emission_mode, "external_import");
    assert_eq!(export_specifier.as_deref(), Some("pkg/add.js"));
    assert_eq!(
        external_import_policy_version,
        PACKAGE_ATTRIBUTION_EXTERNAL_IMPORT_POLICY_VERSION
    );
    assert_eq!(rejection_reason, None);
}

#[test]
fn match_packages_without_filter_revalidates_existing_accepted_attribution() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        "function localOnly(){return 1;}",
        &[(
            "pkg",
            "1.2.3",
            "add.js",
            "export function add(a, b) {\n  return a + b;\n}",
        )],
    );
    connection
        .execute(
            r"
            INSERT INTO package_attributions
                (module_id, module_original_name, package_name, package_version,
                 package_subpath, resolved_file, export_specifier, emission_mode,
                 status, evidence_json, rejection_reason, created_at, updated_at)
            VALUES (10, 'm10', 'pkg', '1.2.3',
                    'add.js', 'pkg@1.2.3/add.js', 'pkg/add.js',
                    'external_import', 'accepted', '{}', NULL, 'old', 'old')
            ",
            [],
        )
        .expect("seed accepted attribution");
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: true,
        package_names: Vec::new(),
        package_source_roots: Vec::new(),
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
    assert_eq!(
        package_attribution_count(&connection),
        1,
        "full revalidation should overwrite the stale row instead of adding a duplicate"
    );
    let (status, emission_mode, export_specifier, external_import_policy_version): (
        String,
        String,
        Option<String>,
        i64,
    ) = connection
        .query_row(
            r"
            SELECT status, emission_mode, export_specifier,
                   external_import_policy_version
              FROM package_attributions
             WHERE module_id = 10
            ",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("accepted attribution should be rewritten");
    assert_eq!(status, "accepted");
    assert_eq!(emission_mode, "external_import");
    assert_eq!(export_specifier.as_deref(), Some("pkg/add.js"));
    assert_eq!(
        external_import_policy_version,
        PACKAGE_ATTRIBUTION_EXTERNAL_IMPORT_POLICY_VERSION
    );
}

#[test]
fn match_packages_reports_and_skips_invalid_package_module_slice() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        "lts.allowAbsoluteUrls !== void 0) K.allowAbsoluteU",
        &[(
            "pkg",
            "1.2.3",
            "add.js",
            "export function add(a, b) {\n  return a + b;\n}",
        )],
    );
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: false,
        package_names: Vec::new(),
        package_source_roots: Vec::new(),
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert!(
        outcome.audit.has(FindingCode::AstFactExtractionFailed),
        "{:?}",
        outcome.audit.findings()
    );
    assert_eq!(outcome.package_source_quality_trusted, 0);
    assert_eq!(outcome.package_source_quality_invalid, 1);
    assert_eq!(
        outcome.matched_modules, 0,
        "invalid source must not be forced to an external package import"
    );
    assert_eq!(outcome.function_ownership_matches, 0);
}

#[test]
fn match_packages_rejects_trailing_garbage_package_module_slice() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        "export function add(a,b){return a+b} %%% trailing-runtime-garbage",
        &[(
            "pkg",
            "1.2.3",
            "add.js",
            "export function add(a, b) {\n  return a + b;\n}",
        )],
    );
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: false,
        package_names: Vec::new(),
        package_source_roots: Vec::new(),
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert!(
        outcome.audit.has(FindingCode::AstFactExtractionFailed),
        "{:?}",
        outcome.audit.findings()
    );
    assert_eq!(
        outcome.package_source_quality_trusted, 0,
        "invalid package slices must not be rewritten before quality counting"
    );
    assert_eq!(outcome.package_source_quality_invalid, 1);
    assert_eq!(outcome.matched_modules, 0);
}

#[test]
fn match_packages_externalizes_unrestricted_subpath_from_package_source_root() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let package_dir = tempdir.path().join("project/node_modules/pkg");
    fs::create_dir_all(package_dir.join("lib").as_path()).expect("create package lib dir");
    fs::create_dir_all(package_dir.join("tests").as_path()).expect("create package test dir");
    fs::write(
        package_dir.join("package.json"),
        r#"{"name":"pkg","version":"1.2.3"}"#,
    )
    .expect("write package json");
    fs::write(
        package_dir.join("lib/add.js"),
        "export function add(a, b) {\n  return a + b;\n}",
    )
    .expect("write package source");
    fs::write(
        package_dir.join("tests/add.test.js"),
        "export function testOnly() { return 'skip'; }",
    )
    .expect("write skipped package test source");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        "export function add(a,b){return a+b}",
        &[],
    );
    connection
        .execute(
            "UPDATE modules SET package_version = '1.2.3' WHERE id = 10",
            [],
        )
        .expect("set exact package version");
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: true,
        package_names: vec!["pkg".to_string()],
        package_source_roots: vec![tempdir.path().join("project")],
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
    assert_eq!(
        outcome.loaded_package_sources, 2,
        "lib/add.js + the root package.json (kept for surface resolution) should load; \
         tests/add.test.js must be skipped (which would make 3)"
    );
    assert_eq!(outcome.matched_modules, 1);
    assert!(
        outcome.function_ownership_matches >= 1,
        "unrestricted subpath roots should still produce ownership evidence"
    );
    assert_eq!(
        outcome.written_attributions, 1,
        "unrestricted package subpaths should be persisted as external imports"
    );
    assert!(
        outcome.function_attributions >= 1,
        "importable unrestricted package subpaths should feed external function attribution"
    );
    assert!(outcome.written_function_attributions >= 1);
    let (status, emission_mode, export_specifier, package_version): (
        String,
        String,
        String,
        Option<String>,
    ) = connection
        .query_row(
            r"
            SELECT status, emission_mode, export_specifier, package_version
              FROM package_attributions
             WHERE module_id = 10
            ",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("unrestricted subpath match should write an external attribution");
    assert_eq!(status, "accepted");
    assert_eq!(emission_mode, "external_import");
    assert_eq!(export_specifier, "pkg/lib/add.js");
    assert_eq!(package_version.as_deref(), Some("1.2.3"));
}

#[test]
fn match_packages_externalizes_public_export_from_package_source_root() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let package_dir = tempdir.path().join("project/node_modules/pkg");
    fs::create_dir_all(package_dir.join("lib").as_path()).expect("create package lib dir");
    fs::write(
        package_dir.join("package.json"),
        r#"{"name":"pkg","version":"1.2.3","exports":{"./add":"./lib/add.js"}}"#,
    )
    .expect("write package json");
    fs::write(
        package_dir.join("lib/add.js"),
        "export function add(a, b) {\n  return a + b;\n}",
    )
    .expect("write package source");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        "export function add(a,b){return a+b}",
        &[],
    );
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: true,
        package_names: vec!["pkg".to_string()],
        package_source_roots: vec![tempdir.path().join("project")],
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
    // package.json is now loaded as a source (read for cache-anchored surface
    // resolution) alongside the importable lib/add.js, hence 2.
    assert_eq!(outcome.loaded_package_sources, 2);
    assert_eq!(outcome.matched_modules, 1);
    assert!(outcome.function_ownership_matches >= 1);
    assert_eq!(outcome.written_attributions, 1);
    let (status, emission_mode, package_version, export_specifier): (
        String,
        String,
        String,
        String,
    ) = connection
        .query_row(
            r"
            SELECT status, emission_mode, package_version, export_specifier
              FROM package_attributions
             WHERE module_id = 10
            ",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("public export should be externalized");
    assert_eq!(status, "accepted");
    assert_eq!(emission_mode, "external_import");
    assert_eq!(package_version, "1.2.3");
    assert_eq!(export_specifier, "pkg/add");
}

#[test]
fn match_packages_externalizes_exported_package_json_from_package_source_root() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let package_dir = tempdir.path().join("project/node_modules/pkg");
    fs::create_dir_all(package_dir.as_path()).expect("create package dir");
    let package_json =
        r#"{"name":"pkg","version":"1.2.3","exports":{"./package.json":"./package.json"}}"#;
    fs::write(package_dir.join("package.json"), package_json).expect("write package json");
    let bundled_source =
        json_package_source_module(package_json).expect("json package source module");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        bundled_source.as_str(),
        &[],
    );
    connection
        .execute(
            "UPDATE modules SET semantic_name = 'pkg/package-json.ts', package_version = '1.2.3' WHERE id = 10",
            [],
        )
        .expect("set package json semantic path");
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: true,
        package_names: vec!["pkg".to_string()],
        package_source_roots: vec![tempdir.path().join("project")],
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
    assert_eq!(outcome.loaded_package_sources, 1);
    assert_eq!(outcome.matched_modules, 1);
    assert_eq!(outcome.written_attributions, 1);
    let (status, emission_mode, export_specifier, resolved_file): (String, String, String, String) =
        connection
            .query_row(
                r"
            SELECT status, emission_mode, export_specifier, resolved_file
              FROM package_attributions
             WHERE module_id = 10
            ",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("package.json export should be externalized");
    assert_eq!(status, "accepted");
    assert_eq!(emission_mode, "external_import");
    assert_eq!(export_specifier, "pkg/package.json");
    assert!(resolved_file.ends_with("package.json"));
}

#[test]
fn match_packages_externalizes_package_needed_by_different_package_consumer() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let first_module = "export function init(){return 1;}";
    let second_module = "\nexport const consumer = init();";
    let bundled_source = format!("{first_module}{second_module}");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        bundled_source.as_str(),
        &[("pkg", "1.2.3", "index.js", first_module)],
    );
    connection
        .execute(
            r"
            UPDATE modules
               SET original_name = 'init',
                   semantic_name = 'pkg/index.js',
                   package_version = '1.2.3',
                   byte_start = 0,
                   byte_end = ?1
             WHERE id = 10
            ",
            [first_module.len() as i64],
        )
        .expect("narrow package module");
    connection
        .execute(
            r"
            INSERT INTO modules
                (id, file_id, original_name, semantic_name, module_category,
                 package_name, package_version, byte_start, byte_end)
            VALUES (11, 1, 'consumer', 'other/consumer.js', 'package',
                    'other', '1.0.0', ?1, ?2)
            ",
            [first_module.len() as i64, bundled_source.len() as i64],
        )
        .expect("insert package consumer");
    connection
        .execute(
            "INSERT INTO module_dependencies (module_id, dependency_id) VALUES (11, 10)",
            [],
        )
        .expect("insert package dependency");
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: true,
        package_names: vec!["pkg".to_string()],
        package_source_roots: Vec::new(),
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert_eq!(outcome.matched_modules, 1);
    assert_eq!(
        connection
            .query_row(
                r"
                SELECT COUNT(*)
                  FROM package_attributions
                 WHERE module_id = 10
                   AND status = 'accepted'
                   AND emission_mode = 'external_import'
                ",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count accepted external"),
        1,
        "a different-package source consumer is a package boundary; the consumer remains source while the producer can be imported externally"
    );
}

#[test]
fn external_import_safety_preserves_unproven_same_package_source_boundary() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "root",
        "pkg/root.ts",
        "pkg",
        Some("1.0.0".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(11),
        "unprovenConsumer",
        "pkg/private-consumer.ts",
        "pkg",
        Some("1.0.0".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(12),
        "leaf",
        "pkg/leaf.ts",
        "pkg",
        Some("1.0.0".to_string()),
    ));
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(10),
        target: ModuleDependencyTarget::Module(ModuleId(11)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(11),
        target: ModuleDependencyTarget::Module(ModuleId(12)),
    });

    let mut report = VersionedPackageMatchReport {
        attributions: vec![
            PackageAttributionInput::accepted_external(ModuleId(10), "pkg", "1.0.0", "pkg"),
            PackageAttributionInput::accepted_external(ModuleId(12), "pkg", "1.0.0", "pkg/leaf"),
        ],
        surfaces: Vec::new(),
        matches: vec![
            package_match(ModuleId(10), "pkg"),
            package_match(ModuleId(12), "pkg/leaf"),
        ],
        version_matches: Vec::new(),
        audit: AuditReport::default(),
    };

    let safety = filter_unsafe_interpackage_external_attributions(&rows, &mut report);

    assert_eq!(safety.removed_modules, 0);
    assert!(safety.blockers.is_empty());
    assert_eq!(
        report
            .attributions
            .iter()
            .map(|attribution| attribution.module_id)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([ModuleId(10), ModuleId(12)]),
        "unproven same-package consumers are preserved as source boundaries rather than source-suppressed"
    );
    assert_eq!(
        source_eliminated_package_modules_for_report(&rows, &report),
        2,
        "only the two direct external imports are eliminated; the unproven consumer is not source-suppressed"
    );
    let proofs = externalization_chain_proofs(&rows, &report);
    let leaf_proof = proofs.get(&ModuleId(12)).expect("leaf chain proof");
    assert!(
        leaf_proof
            .get("incoming_consumers")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .any(|consumer| {
                consumer
                    .get("resolution")
                    .and_then(serde_json::Value::as_str)
                    == Some("source_boundary")
            }),
        "chain proof should record preserved same-package consumers as source boundaries"
    );
}

#[test]
fn external_import_safety_preserves_cyclic_same_package_source_boundary() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "public",
        "pkg/public.ts",
        "pkg",
        Some("1.0.0".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(11),
        "runtimeA",
        "pkg/runtime-a.ts",
        "pkg",
        Some("1.0.0".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(12),
        "runtimeB",
        "pkg/runtime-b.ts",
        "pkg",
        Some("1.0.0".to_string()),
    ));
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(11),
        target: ModuleDependencyTarget::Module(ModuleId(10)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(11),
        target: ModuleDependencyTarget::Module(ModuleId(12)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(12),
        target: ModuleDependencyTarget::Module(ModuleId(11)),
    });

    let mut report = VersionedPackageMatchReport {
        attributions: vec![PackageAttributionInput::accepted_external(
            ModuleId(10),
            "pkg",
            "1.0.0",
            "pkg/public",
        )],
        surfaces: Vec::new(),
        matches: vec![package_match(ModuleId(10), "pkg/public")],
        version_matches: Vec::new(),
        audit: AuditReport::default(),
    };

    let safety = filter_unsafe_interpackage_external_attributions(&rows, &mut report);

    assert_eq!(safety.removed_modules, 0);
    let proofs = externalization_chain_proofs(&rows, &report);
    assert!(
        proofs
            .get(&ModuleId(10))
            .and_then(|proof| proof.get("incoming_consumers"))
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .any(|consumer| {
                consumer
                    .get("resolution")
                    .and_then(serde_json::Value::as_str)
                    == Some("source_boundary")
            }),
        "closed same-package source cycles are preserved as source boundaries, not blockers"
    );
}

#[test]
fn external_import_safety_allows_source_suppressed_package_closure_consumers() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "root",
        "pkg/root.ts",
        "pkg",
        Some("1.0.0".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(11),
        "privateConsumer",
        "pkg/private-consumer.ts",
        "pkg",
        Some("1.0.0".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(12),
        "leaf",
        "pkg/leaf.ts",
        "pkg",
        Some("1.0.0".to_string()),
    ));
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(10),
        target: ModuleDependencyTarget::Module(ModuleId(11)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(11),
        target: ModuleDependencyTarget::Module(ModuleId(12)),
    });
    rows.package_attributions
        .push(rejected_package_ownership(ModuleId(11), "pkg", "1.0.0"));

    let mut report = VersionedPackageMatchReport {
        attributions: vec![
            PackageAttributionInput::accepted_external(ModuleId(10), "pkg", "1.0.0", "pkg"),
            PackageAttributionInput::accepted_external(ModuleId(12), "pkg", "1.0.0", "pkg/leaf"),
        ],
        surfaces: Vec::new(),
        matches: vec![
            package_match(ModuleId(10), "pkg"),
            package_match(ModuleId(12), "pkg/leaf"),
        ],
        version_matches: Vec::new(),
        audit: AuditReport::default(),
    };

    let safety = filter_unsafe_interpackage_external_attributions(&rows, &mut report);

    assert_eq!(safety.removed_modules, 0);
    assert_eq!(
        report
            .attributions
            .iter()
            .map(|attribution| attribution.module_id)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([ModuleId(10), ModuleId(12)]),
        "a private package consumer that is only reachable from an externalized root is suppressed with that closure"
    );
    let proofs = externalization_chain_proofs(&rows, &report);
    let root_proof = proofs.get(&ModuleId(10)).expect("root chain proof");
    assert_eq!(
        root_proof
            .get("source_suppressed_dependency_count")
            .and_then(serde_json::Value::as_u64),
        Some(1)
    );
    let leaf_proof = proofs.get(&ModuleId(12)).expect("leaf chain proof");
    assert!(
        leaf_proof
            .get("incoming_consumers")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .any(|consumer| {
                consumer
                    .get("resolution")
                    .and_then(serde_json::Value::as_str)
                    == Some("source_suppressed")
            }),
        "leaf proof should record that its private consumer is source-suppressed"
    );
}

#[test]
fn external_import_safety_allows_application_boundary_consumers() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "root",
        "pkg/root.ts",
        "pkg",
        Some("1.0.0".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(11),
        "privateConsumer",
        "pkg/private-consumer.ts",
        "pkg",
        Some("1.0.0".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(12),
        "leaf",
        "pkg/leaf.ts",
        "pkg",
        Some("1.0.0".to_string()),
    ));
    rows.modules
        .push(ModuleInput::application(ModuleId(20), "app", "app.ts"));
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(10),
        target: ModuleDependencyTarget::Module(ModuleId(11)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(11),
        target: ModuleDependencyTarget::Module(ModuleId(12)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(20),
        target: ModuleDependencyTarget::Module(ModuleId(12)),
    });
    rows.package_attributions
        .push(rejected_package_ownership(ModuleId(11), "pkg", "1.0.0"));

    let mut report = VersionedPackageMatchReport {
        attributions: vec![
            PackageAttributionInput::accepted_external(ModuleId(10), "pkg", "1.0.0", "pkg"),
            PackageAttributionInput::accepted_external(ModuleId(12), "pkg", "1.0.0", "pkg/leaf"),
        ],
        surfaces: Vec::new(),
        matches: vec![
            package_match(ModuleId(10), "pkg"),
            package_match(ModuleId(12), "pkg/leaf"),
        ],
        version_matches: Vec::new(),
        audit: AuditReport::default(),
    };

    let safety = filter_unsafe_interpackage_external_attributions(&rows, &mut report);

    assert_eq!(safety.removed_modules, 0);
    assert_eq!(
        report
            .attributions
            .iter()
            .map(|attribution| attribution.module_id)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([ModuleId(10), ModuleId(12)]),
        "application modules are a boundary consumer: package consumers still need chain proof, but app code may consume the external package adapter"
    );
    let proofs = externalization_chain_proofs(&rows, &report);
    let leaf_proof = proofs.get(&ModuleId(12)).expect("leaf chain proof");
    assert!(
        leaf_proof
            .get("incoming_consumers")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .any(|consumer| {
                consumer
                    .get("resolution")
                    .and_then(serde_json::Value::as_str)
                    == Some("application_boundary")
            }),
        "chain proof should record application consumers as boundary consumers"
    );
}

#[test]
fn external_import_safety_allows_builtin_source_boundary_consumers() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "public",
        "pkg/public.ts",
        "pkg",
        Some("1.0.0".to_string()),
    ));
    let mut builtin = ModuleInput::application(ModuleId(20), "node", "node/module.ts");
    builtin.kind = ModuleKind::Builtin;
    rows.modules.push(builtin);
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(20),
        target: ModuleDependencyTarget::Module(ModuleId(10)),
    });

    let mut report = VersionedPackageMatchReport {
        attributions: vec![PackageAttributionInput::accepted_external(
            ModuleId(10),
            "pkg",
            "1.0.0",
            "pkg/public",
        )],
        surfaces: Vec::new(),
        matches: vec![package_match(ModuleId(10), "pkg/public")],
        version_matches: Vec::new(),
        audit: AuditReport::default(),
    };

    let safety = filter_unsafe_interpackage_external_attributions(&rows, &mut report);

    assert_eq!(safety.removed_modules, 0);
    let proofs = externalization_chain_proofs(&rows, &report);
    assert!(
        proofs
            .get(&ModuleId(10))
            .and_then(|proof| proof.get("incoming_consumers"))
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .any(|consumer| {
                consumer
                    .get("resolution")
                    .and_then(serde_json::Value::as_str)
                    == Some("builtin_boundary")
            }),
        "builtin shim modules are preserved source boundaries for direct external imports"
    );
}

#[test]
fn source_suppression_does_not_eliminate_private_package_needed_by_builtin_consumer() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "root",
        "pkg/root.ts",
        "pkg",
        Some("1.0.0".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(11),
        "private",
        "pkg/private.ts",
        "pkg",
        Some("1.0.0".to_string()),
    ));
    let mut builtin = ModuleInput::application(ModuleId(20), "node", "node/module.ts");
    builtin.kind = ModuleKind::Builtin;
    rows.modules.push(builtin);
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(10),
        target: ModuleDependencyTarget::Module(ModuleId(11)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(20),
        target: ModuleDependencyTarget::Module(ModuleId(11)),
    });
    rows.package_attributions
        .push(rejected_package_ownership(ModuleId(11), "pkg", "1.0.0"));

    let report = VersionedPackageMatchReport {
        attributions: vec![PackageAttributionInput::accepted_external(
            ModuleId(10),
            "pkg",
            "1.0.0",
            "pkg",
        )],
        surfaces: Vec::new(),
        matches: vec![package_match(ModuleId(10), "pkg")],
        version_matches: Vec::new(),
        audit: AuditReport::default(),
    };

    assert_eq!(
        source_eliminated_package_modules_for_report(&rows, &report),
        1,
        "private transitive package sources are only suppressed when every consumer can be removed or is an application/package boundary"
    );
}

#[test]
fn external_import_safety_allows_different_package_boundary_consumers() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "public",
        "pkg-a/public.ts",
        "pkg-a",
        Some("1.0.0".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(20),
        "consumer",
        "pkg-b/consumer.ts",
        "pkg-b",
        Some("1.0.0".to_string()),
    ));
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(20),
        target: ModuleDependencyTarget::Module(ModuleId(10)),
    });

    let mut report = VersionedPackageMatchReport {
        attributions: vec![PackageAttributionInput::accepted_external(
            ModuleId(10),
            "pkg-a",
            "1.0.0",
            "pkg-a",
        )],
        surfaces: Vec::new(),
        matches: vec![package_match(ModuleId(10), "pkg-a")],
        version_matches: Vec::new(),
        audit: AuditReport::default(),
    };

    let safety = filter_unsafe_interpackage_external_attributions(&rows, &mut report);

    assert_eq!(safety.removed_modules, 0);
    let proofs = externalization_chain_proofs(&rows, &report);
    assert!(
        proofs
            .get(&ModuleId(10))
            .and_then(|proof| proof.get("incoming_consumers"))
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .any(|consumer| {
                consumer
                    .get("resolution")
                    .and_then(serde_json::Value::as_str)
                    == Some("package_boundary")
            }),
        "different package source consumers are boundary consumers; only same-package consumers must be externalized or source-suppressed"
    );
}

#[test]
fn match_packages_externalizes_wildcard_export_from_package_source_root() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let package_dir = tempdir.path().join("project/node_modules/pkg");
    fs::create_dir_all(package_dir.join("lib/features").as_path())
        .expect("create package feature dir");
    fs::write(
        package_dir.join("package.json"),
        r#"{"name":"pkg","version":"1.2.3","exports":{"./features/*":"./lib/features/*.js"}}"#,
    )
    .expect("write package json");
    fs::write(
        package_dir.join("lib/features/add.js"),
        "export function add(a, b) {\n  return a + b;\n}",
    )
    .expect("write package source");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        "export function add(a,b){return a+b}",
        &[],
    );
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: true,
        package_names: vec!["pkg".to_string()],
        package_source_roots: vec![tempdir.path().join("project")],
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
    // 2 = the wildcard-matched module source + the root package.json (kept for
    // cache-anchored surface resolution).
    assert_eq!(outcome.loaded_package_sources, 2);
    assert_eq!(outcome.matched_modules, 1);
    assert!(outcome.function_ownership_matches >= 1);
    let (status, emission_mode, package_version, export_specifier): (
        String,
        String,
        String,
        String,
    ) = connection
        .query_row(
            r"
            SELECT status, emission_mode, package_version, export_specifier
              FROM package_attributions
             WHERE module_id = 10
            ",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("wildcard export should be externalized");
    assert_eq!(status, "accepted");
    assert_eq!(emission_mode, "external_import");
    assert_eq!(package_version, "1.2.3");
    assert_eq!(export_specifier, "pkg/features/add");
}

#[test]
fn match_packages_externalizes_conditional_wildcard_export_from_package_source_root() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let package_dir = tempdir.path().join("project/node_modules/pkg");
    fs::create_dir_all(package_dir.join("cjs/features").as_path())
        .expect("create package cjs feature dir");
    fs::create_dir_all(package_dir.join("esm/features").as_path())
        .expect("create package esm feature dir");
    fs::write(
        package_dir.join("package.json"),
        r#"{"name":"pkg","version":"1.2.3","exports":{"./features/*":{"require":"./cjs/features/*.cjs","import":"./esm/features/*.js"}}}"#,
    )
    .expect("write package json");
    fs::write(
        package_dir.join("cjs/features/add.cjs"),
        "exports.add = function add(a, b) {\n  return a + b;\n};",
    )
    .expect("write package cjs source");
    fs::write(
        package_dir.join("esm/features/add.js"),
        "export function add(a, b) {\n  return a + b;\n}",
    )
    .expect("write package esm source");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        "export function add(a,b){return a+b}",
        &[],
    );
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: true,
        package_names: vec!["pkg".to_string()],
        package_source_roots: vec![tempdir.path().join("project")],
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
    // 3 = the two conditional wildcard module sources + the root package.json.
    assert_eq!(outcome.loaded_package_sources, 3);
    assert_eq!(outcome.matched_modules, 1);
    let export_specifier: String = connection
        .query_row(
            r"
            SELECT export_specifier
              FROM package_attributions
             WHERE module_id = 10
               AND status = 'accepted'
               AND emission_mode = 'external_import'
            ",
            [],
            |row| row.get(0),
        )
        .expect("conditional wildcard export should be externalized");
    assert_eq!(export_specifier, "pkg/features/add");
}

#[test]
fn match_packages_forces_require_only_conditional_export_external() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let package_dir = tempdir.path().join("project/node_modules/pkg");
    fs::create_dir_all(package_dir.join("cjs/features").as_path())
        .expect("create package cjs feature dir");
    fs::write(
        package_dir.join("package.json"),
        r#"{"name":"pkg","version":"1.2.3","exports":{"./features/*":{"require":"./cjs/features/*.cjs"}}}"#,
    )
    .expect("write package json");
    fs::write(
        package_dir.join("cjs/features/add.cjs"),
        "exports.add = function add(a, b) {\n  return a + b;\n};",
    )
    .expect("write package source");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        "exports.add=function add(a,b){return a+b};",
        &[],
    );
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: true,
        package_names: vec!["pkg".to_string()],
        package_source_roots: vec![tempdir.path().join("project")],
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
    // 2 = the matched module source + the root package.json.
    assert_eq!(outcome.loaded_package_sources, 2);
    assert_eq!(outcome.matched_modules, 1);
    assert_eq!(
        outcome.function_attributions, 0,
        "forced module externalization does not imply function-level external evidence"
    );
    let (status, emission_mode, package_version, export_specifier, rejection_reason): (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = connection
        .query_row(
            r"
            SELECT status, emission_mode, package_version, export_specifier, rejection_reason
              FROM package_attributions
             WHERE module_id = 10
            ",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .expect("require-only export should write a rejected ownership attribution");
    assert_eq!(status, "rejected");
    assert_eq!(emission_mode, "application_source");
    assert_eq!(package_version.as_deref(), Some("1.2.3"));
    assert_eq!(export_specifier, None);
    assert!(rejection_reason.is_some());
}

#[test]
fn match_packages_forces_ambiguous_wildcard_export_external() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let package_dir = tempdir.path().join("project/node_modules/pkg");
    fs::create_dir_all(package_dir.join("lib").as_path()).expect("create package lib dir");
    fs::write(
        package_dir.join("package.json"),
        r#"{"name":"pkg","version":"1.2.3","exports":{"./a/*":"./lib/*.js","./b/*":"./lib/*.js"}}"#,
    )
    .expect("write package json");
    fs::write(
        package_dir.join("lib/add.js"),
        "export function add(a, b) {\n  return a + b;\n}",
    )
    .expect("write package source");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        "export function add(a,b){return a+b}",
        &[],
    );
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: true,
        package_names: vec!["pkg".to_string()],
        package_source_roots: vec![tempdir.path().join("project")],
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
    // 2 = the matched module source + the root package.json.
    assert_eq!(outcome.loaded_package_sources, 2);
    assert_eq!(outcome.matched_modules, 1);
    assert_eq!(
        outcome.function_attributions, 0,
        "forced module externalization does not imply function-level external evidence"
    );
    let (status, emission_mode, package_version, export_specifier): (
        String,
        String,
        Option<String>,
        Option<String>,
    ) = connection
        .query_row(
            r"
            SELECT status, emission_mode, package_version, export_specifier
              FROM package_attributions
             WHERE module_id = 10
            ",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("ambiguous wildcard export should write a rejected ownership attribution");
    assert_eq!(status, "rejected");
    assert_eq!(emission_mode, "application_source");
    assert_eq!(package_version.as_deref(), Some("1.2.3"));
    assert_eq!(export_specifier, None);
}

#[test]
fn match_packages_uses_package_source_root_without_cache_table() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let package_dir = tempdir.path().join("node_modules/pkg");
    fs::create_dir_all(package_dir.join("lib").as_path()).expect("create package lib dir");
    fs::write(
        package_dir.join("package.json"),
        r#"{"name":"pkg","version":"1.2.3"}"#,
    )
    .expect("write package json");
    fs::write(
        package_dir.join("lib/add.js"),
        "export function add(a, b) {\n  return a + b;\n}",
    )
    .expect("write package source");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        "export function add(a,b){return a+b}",
        &[],
    );
    connection
        .execute("DROP TABLE package_source_cache", [])
        .expect("drop cache table");
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: false,
        package_names: vec!["pkg".to_string()],
        package_source_roots: vec![tempdir.path().to_path_buf()],
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
    // 2 = the matched module source + the root package.json.
    assert_eq!(outcome.loaded_package_sources, 2);
    assert_eq!(outcome.matched_modules, 1);
    assert!(
        outcome.function_attributions >= 1,
        "unrestricted subpath package source should be importable even in dry-run"
    );
    assert_eq!(package_attribution_count(&connection), 0);
}

#[test]
fn match_packages_promotes_full_cascade_function_coverage_to_module_attribution() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        "module.exports = function add(a,b){return a+b};",
        &[(
            "pkg",
            "1.2.3",
            "add.js",
            "const add = function add(a, b) {\n  return a + b;\n};",
        )],
    );
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: true,
        package_names: Vec::new(),
        package_source_roots: Vec::new(),
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
    assert_eq!(
        outcome.matched_modules, 1,
        "module-level attribution should be backed by function evidence"
    );
    assert_eq!(outcome.written_attributions, 1);
    assert!(outcome.written_function_attributions >= 1);
    let (status, emission_mode, package_version, evidence): (String, String, String, String) =
        connection
            .query_row(
                r"
            SELECT status, emission_mode, package_version, evidence_json
              FROM package_attributions
             WHERE module_id = 10
            ",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("cascade module attribution should be written");
    assert_eq!(status, "accepted");
    assert_eq!(emission_mode, "external_import");
    assert_eq!(package_version, "1.2.3");
    assert!(evidence.contains("cascade_function_coverage"));
}

#[test]
fn match_packages_forces_source_only_cascade_ownership_external() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let package_dir = tempdir.path().join("project/node_modules/pkg");
    fs::create_dir_all(package_dir.join("lib").as_path()).expect("create package lib dir");
    fs::write(
        package_dir.join("package.json"),
        r#"{"name":"pkg","version":"1.2.3","exports":{"./add":{"require":"./lib/add.js"}}}"#,
    )
    .expect("write package json");
    fs::write(
        package_dir.join("lib/add.js"),
        "const add = function add(a, b) {\n  return a + b;\n};",
    )
    .expect("write package source");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        "module.exports = function add(a,b){return a+b};",
        &[],
    );
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: true,
        package_names: vec!["pkg".to_string()],
        package_source_roots: vec![tempdir.path().join("project")],
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
    // 2 = the matched module source + the root package.json.
    assert_eq!(outcome.loaded_package_sources, 2);
    assert_eq!(
        outcome.matched_modules, 1,
        "source-only cascade coverage should still count as package ownership"
    );
    assert_eq!(
        outcome.function_ownership_matches, 1,
        "the source-only function should still produce one ownership match"
    );
    assert_eq!(
        outcome.function_attributions, 0,
        "source-only ownership must not become function-level external attributions"
    );
    assert_eq!(outcome.written_function_attributions, 0);
    assert_eq!(
        outcome.written_attributions, 1,
        "the module should receive a rejected ownership decision when the import target is unsafe"
    );

    let (status, emission_mode, package_version, export_specifier, rejection_reason, evidence): (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
    ) = connection
        .query_row(
            r"
            SELECT status, emission_mode, package_version, export_specifier,
                   rejection_reason, evidence_json
              FROM package_attributions
             WHERE module_id = 10
            ",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .expect("source-only function ownership should write a rejected attribution");
    assert_eq!(status, "rejected");
    assert_eq!(emission_mode, "application_source");
    assert_eq!(package_version.as_deref(), Some("1.2.3"));
    assert_eq!(export_specifier, None);
    assert!(rejection_reason.is_some());
    assert!(evidence.contains("cascade_function_coverage"));
}

#[test]
fn match_packages_forces_structural_bag_ownership_external() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        r#"
        function a(x){if(x){return true;}return false;}
        function b(y){if(y){return true;}return false;}
        "#,
        &[(
            "pkg",
            "1.2.3",
            "combined.js",
            r#"
            function first(value){if(value){return true;}return false;}
            function second(input){if(input){return true;}return false;}
            "#,
        )],
    );
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: true,
        package_names: Vec::new(),
        package_source_roots: Vec::new(),
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
    assert_eq!(
        outcome.matched_modules, 1,
        "structural bag evidence should be promoted as source-only ownership"
    );
    assert_eq!(
        outcome.function_ownership_matches, 0,
        "this fixture should not be matched by cascade"
    );
    assert_eq!(
        outcome.written_attributions, 1,
        "unmatched package modules still receive an explicit rejected source decision"
    );
    let (attribution_count, evidence): (i64, String) = connection
        .query_row(
            r"
            SELECT COUNT(*), COALESCE(MAX(evidence_json), '')
              FROM package_attributions
             WHERE module_id = 10
            ",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("count attribution rows");
    assert_eq!(attribution_count, 1);
    assert!(evidence.contains("aggregate_structural_bag_similarity"));
    assert!(evidence.contains("structural-bag:pkg@1.2.3"));
}

#[test]
fn match_packages_promotes_dependency_closure_ownership_for_wrapper() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let wrapper_path = tempdir.path().join("wrapper.js");
    let one_path = tempdir.path().join("one.js");
    let two_path = tempdir.path().join("two.js");
    let mut connection = package_match_connection(
        wrapper_path.clone(),
        "var wrap = E(() => { one(); two(); });",
        &[
            (
                "pkg",
                "1.2.3",
                "one.js",
                "export function one(){return 'one-anchor';}",
            ),
            (
                "pkg",
                "1.2.3",
                "two.js",
                "export function two(){return 'two-anchor';}",
            ),
        ],
    );
    fs::write(
        one_path.as_path(),
        "export function one(){return 'one-anchor';}",
    )
    .expect("write one source");
    fs::write(
        two_path.as_path(),
        "export function two(){return 'two-anchor';}",
    )
    .expect("write two source");
    connection
        .execute(
            "INSERT INTO source_files (id, file_path) VALUES (2, ?1), (3, ?2)",
            params![
                one_path.to_string_lossy().as_ref(),
                two_path.to_string_lossy().as_ref()
            ],
        )
        .expect("insert source files");
    connection
        .execute(
            "INSERT INTO project_files (project_id, file_id) VALUES (1, 2), (1, 3)",
            [],
        )
        .expect("insert project files");
    connection
        .execute_batch(
            r"
            UPDATE modules
               SET semantic_name = 'pkg/wrapper.js',
                   byte_start = NULL,
                   byte_end = NULL
             WHERE id = 10;
            INSERT INTO modules
                (id, file_id, original_name, semantic_name, module_category,
                 package_name, package_version, byte_start, byte_end)
            VALUES
                (11, 2, 'one', 'pkg/one.js', 'package', 'pkg', '1.2.3', NULL, NULL),
                (12, 3, 'two', 'pkg/two.js', 'package', 'pkg', '1.2.3', NULL, NULL);
            INSERT INTO module_dependencies (module_id, dependency_id)
            VALUES (10, 11), (10, 12);
            ",
        )
        .expect("seed dependency closure fixture");
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: true,
        package_names: Vec::new(),
        package_source_roots: Vec::new(),
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
    assert_eq!(outcome.matched_modules, 3);
    assert_eq!(outcome.written_attributions, 3);
    let (status, emission_mode, package_version, evidence): (
        String,
        String,
        Option<String>,
        String,
    ) = connection
        .query_row(
            r"
            SELECT status, emission_mode, package_version, evidence_json
              FROM package_attributions
             WHERE module_id = 10
            ",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("wrapper should have rejected ownership attribution");
    assert_eq!(status, "rejected");
    assert_eq!(emission_mode, "application_source");
    assert_eq!(package_version.as_deref(), Some("1.2.3"));
    assert!(evidence.contains("dependency_closure_ownership"));
    assert!(evidence.contains("exact-hint:pkg@1.2.3"));
}

#[test]
fn match_packages_resolves_weak_unversioned_hint_to_forced_external() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let dependency_path = tempdir.path().join("axios.js");
    let mut connection = package_match_connection(
        tempdir.path().join("rxjs-wrapper.js"),
        "var r = E(() => { axiosDep(); });",
        &[(
            "rxjs",
            "7.8.2",
            "sample.js",
            "export function sample(notifier){return notifier;}",
        )],
    );
    connection
        .execute(
            "UPDATE modules SET semantic_name = 'rxjs/operators/sample', package_name = 'rxjs', package_version = NULL WHERE id = 10",
            [],
        )
        .expect("make weak rxjs hint");
    fs::write(dependency_path.as_path(), "export const axiosDep = 1;")
        .expect("write dependency source");
    connection
        .execute(
            "INSERT INTO source_files (id, file_path) VALUES (2, ?1)",
            [dependency_path.to_string_lossy().as_ref()],
        )
        .expect("insert dependency source file");
    connection
        .execute(
            "INSERT INTO project_files (project_id, file_id) VALUES (1, 2)",
            [],
        )
        .expect("insert dependency project file");
    connection
        .execute_batch(
            r"
            INSERT INTO modules
                (id, file_id, original_name, semantic_name, module_category,
                 package_name, package_version, byte_start, byte_end)
            VALUES (11, 2, 'axiosDep', 'axios/index.js', 'package', 'axios', '1.7.3', NULL, NULL);
            INSERT INTO module_dependencies (module_id, dependency_id) VALUES (10, 11);
            ",
        )
        .expect("seed contradicted dependency");
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: true,
        package_names: vec!["rxjs".to_string()],
        package_source_roots: Vec::new(),
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
    assert_eq!(outcome.matched_modules, 1);
    assert_eq!(
        outcome.written_attributions, 2,
        "component-scoped package matching also writes the dependency package rejection"
    );
    let (status, emission_mode, package_version, rejection_reason, evidence_json): (
        String,
        String,
        Option<String>,
        Option<String>,
        String,
    ) = connection
        .query_row(
            r"
            SELECT status, emission_mode, package_version, rejection_reason, evidence_json
              FROM package_attributions
             WHERE module_id = 10
            ",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .expect("weak resolved hint should write rejected ownership evidence");
    assert_eq!(status, "rejected");
    assert_eq!(emission_mode, "application_source");
    assert_eq!(package_version.as_deref(), Some("7.8.2"));
    assert!(rejection_reason.is_some());
    assert!(evidence_json.contains("rxjs"));
}

#[test]
fn match_packages_forces_partial_cascade_coverage_external() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        r#"
        function first(value) {
          if (value > 0) {
            return value + 1;
          }
          return 0;
        }
        function second(limit) {
          let total = 0;
          for (let i = 0; i < limit; i++) {
            total += i;
          }
          return total;
        }
        function localOnly() { return 3; }
        exports.value = first(1) + second(2) + localOnly();
        "#,
        &[(
            "pkg",
            "1.2.3",
            "partial.js",
            r#"
            function first(value) {
              if (value > 0) {
                return value + 1;
              }
              return 0;
            }
            function second(limit) {
              let total = 0;
              for (let i = 0; i < limit; i++) {
                total += i;
              }
              return total;
            }
            exports.value = first(1) + second(2);
            "#,
        )],
    );
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: true,
        package_names: Vec::new(),
        package_source_roots: Vec::new(),
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
    assert_eq!(
        outcome.matched_modules, 1,
        "2/3 function ownership should pass the partial cascade threshold"
    );
    assert_eq!(
        outcome.written_attributions, 1,
        "partial ownership should be persisted as rejected source-retention evidence"
    );
    assert!(
        outcome.written_function_attributions >= 2,
        "function-level cascade evidence should still be recorded"
    );
    let (status, emission_mode, package_version, export_specifier, rejection_reason): (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = connection
        .query_row(
            r"
            SELECT status, emission_mode, package_version, export_specifier,
                   rejection_reason
              FROM package_attributions
             WHERE module_id = 10
            ",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .expect("partial function ownership should write a rejected ownership attribution");
    assert_eq!(status, "rejected");
    assert_eq!(emission_mode, "application_source");
    assert_eq!(package_version.as_deref(), Some("1.2.3"));
    assert_eq!(export_specifier, None);
    assert!(rejection_reason.is_some());
}

#[test]
fn match_packages_scopes_cascade_by_module_package_version_hint() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        "module.exports = function add(a,b){return a+b};",
        &[
            (
                "pkg",
                "1.0.0",
                "add.js",
                "const add = function add(a, b) {\n  return a + b;\n};",
            ),
            (
                "pkg",
                "2.0.0",
                "add.js",
                "const add = function add(a, b) {\n  return a + b;\n};",
            ),
        ],
    );
    connection
        .execute(
            "UPDATE modules SET package_version = '2.0.0' WHERE id = 10",
            [],
        )
        .expect("set package version hint");
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: true,
        package_names: Vec::new(),
        package_source_roots: Vec::new(),
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
    assert_eq!(outcome.matched_modules, 1);
    let package_version: String = connection
        .query_row(
            r"
            SELECT package_version
              FROM package_attributions
             WHERE module_id = 10
               AND status = 'accepted'
            ",
            [],
            |row| row.get(0),
        )
        .expect("cascade module attribution should be written");
    assert_eq!(package_version, "2.0.0");
}

#[test]
fn match_packages_package_name_filter_skips_unrequested_modules() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let other_source_path = tempdir.path().join("other.js");
    fs::write(other_source_path.as_path(), "function broken(").expect("write source fixture");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        "export function add(a,b){return a+b}",
        &[(
            "pkg",
            "1.2.3",
            "add.js",
            "export function add(a, b) {\n  return a + b;\n}",
        )],
    );
    connection
        .execute(
            "INSERT INTO source_files (id, file_path) VALUES (2, ?1)",
            [other_source_path.to_string_lossy().as_ref()],
        )
        .expect("insert source file");
    connection
        .execute(
            "INSERT INTO project_files (project_id, file_id) VALUES (1, 2)",
            [],
        )
        .expect("insert project file");
    connection
        .execute(
            r"
            INSERT INTO modules
                (id, file_id, original_name, semantic_name, module_category,
                 package_name, package_version, byte_start, byte_end)
            VALUES (11, 2, 'other', 'other/index.js', 'package', 'other', NULL, 0, ?1)
            ",
            ["function broken(".len() as i64],
        )
        .expect("insert unrequested module");
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: false,
        package_names: vec!["pkg".to_string()],
        package_source_roots: Vec::new(),
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert!(
        outcome.audit.has(FindingCode::AstFactExtractionFailed),
        "{:?}",
        outcome.audit.findings()
    );
    assert_eq!(outcome.loaded_package_modules, 2);
    assert_eq!(outcome.loaded_package_sources, 1);
    assert_eq!(outcome.matched_modules, 1);
    assert_eq!(outcome.matched_package_surfaces, 0);
}

#[test]
fn match_packages_apply_writes_best_version_from_binary_matcher() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        "export function add(a,b){return a+b}",
        &[
            (
                "pkg",
                "2.0.0",
                "add.js",
                "export function sub(a,b){return a-b}",
            ),
            (
                "pkg",
                "1.2.3",
                "add.js",
                "export function add(a, b) {\n  return a + b;\n}",
            ),
        ],
    );
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: true,
        package_names: Vec::new(),
        package_source_roots: Vec::new(),
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");
    let (package_version, evidence): (String, String) = connection
        .query_row(
            "SELECT package_version, evidence_json FROM package_attributions WHERE module_id = 10",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("package attribution should be written");

    assert!(outcome.audit.is_clean());
    assert_eq!(outcome.matched_modules, 1);
    assert_eq!(outcome.matched_package_surfaces, 0);
    assert_eq!(outcome.written_attributions, 1);
    assert_eq!(outcome.written_surfaces, 0);
    assert_eq!(package_version, "1.2.3");
    assert!(evidence.contains("exact_normalized_source_binary_search"));
}

#[test]
fn match_packages_apply_writes_function_attribution() {
    // The assertion looks at the `package_function_attributions` table populated
    // by the function-level matcher. It should produce an Exact-tier match
    // for the bundle's `add` function against the package source, and
    // persist it with function_span + confidence rather than discarding
    // the row.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        "export function add(a,b){return a+b}",
        &[(
            "pkg",
            "1.2.3",
            "add.js",
            "export function add(a, b) {\n  return a + b;\n}",
        )],
    );
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: true,
        package_names: Vec::new(),
        package_source_roots: Vec::new(),
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert!(outcome.audit.is_clean());
    assert!(
        outcome.written_function_attributions >= 1,
        "expected function attribution to be persisted, outcome={:?}",
        outcome,
    );

    let (tier, span_start, span_end, package_name, package_version, matched_axes_json): (
        String,
        i64,
        i64,
        String,
        String,
        String,
    ) = connection
        .query_row(
            r"
            SELECT tier, function_span_start, function_span_end,
                   package_name, package_version, matched_axes_json
              FROM package_function_attributions
             WHERE module_id = 10
            ",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .expect("cascade function attribution row must exist");

    assert_eq!(tier, "exact");
    assert_eq!(package_name, "pkg");
    assert_eq!(package_version, "1.2.3");
    assert!(span_end > span_start);
    assert!(matched_axes_json.contains("ast"));
}

#[test]
fn match_packages_dry_run_does_not_persist_function_attributions() {
    // With apply=false, the function-level matcher still runs (the diagnostic
    // count is non-zero in the outcome), but no rows land in the new
    // function-attributions table.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        "export function add(a,b){return a+b}",
        &[(
            "pkg",
            "1.2.3",
            "add.js",
            "export function add(a, b) {\n  return a + b;\n}",
        )],
    );
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: false,
        package_names: Vec::new(),
        package_source_roots: Vec::new(),
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");

    assert!(
        outcome.function_attributions >= 1,
        "function matcher should compute"
    );
    assert_eq!(outcome.written_function_attributions, 0);
    // The new table should not exist yet since persistence never ran.
    let table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='package_function_attributions'",
            [],
            |row| row.get(0),
        )
        .expect("sqlite_master is always queryable");
    assert_eq!(table_count, 0);
}

#[test]
fn unversioned_package_versions_resolve_to_latest_cached_version() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        "export function add(a,b){return a+b}",
        &[
            (
                "pkg",
                "1.2.3",
                "add.js",
                "export function add(a,b){return a+b}",
            ),
            (
                "pkg",
                "2.0.0",
                "add.js",
                "export function add(a,b){return a+b}",
            ),
        ],
    );
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: true,
        package_names: Vec::new(),
        package_source_roots: Vec::new(),
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");
    let (
        status,
        rejection_reason,
        package_version,
        emission_mode,
        external_import_policy_version,
    ): (String, Option<String>, Option<String>, String, i64) = connection
        .query_row(
            r"
            SELECT status, rejection_reason, package_version, emission_mode,
                   external_import_policy_version
              FROM package_attributions
             WHERE module_id = 10
            ",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .expect("resolved attribution should be written");
    let package_version_not_null: i64 = connection
        .query_row(
            r"
            SELECT [notnull]
              FROM pragma_table_info('package_attributions')
             WHERE name = 'package_version'
            ",
            [],
            |row| row.get(0),
        )
        .expect("package_version column should exist");

    assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
    assert_eq!(outcome.matched_modules, 1);
    assert_eq!(outcome.matched_package_surfaces, 0);
    assert_eq!(outcome.written_attributions, 1);
    assert_eq!(outcome.written_surfaces, 0);
    assert_eq!(package_attribution_count(&connection), 1);
    assert_eq!(status, "accepted");
    assert_eq!(rejection_reason, None);
    assert_eq!(package_version.as_deref(), Some("2.0.0"));
    assert_eq!(emission_mode, "external_import");
    assert_eq!(
        external_import_policy_version,
        PACKAGE_ATTRIBUTION_EXTERNAL_IMPORT_POLICY_VERSION
    );
    assert_eq!(package_version_not_null, 0);
}

#[test]
fn match_packages_apply_replaces_proposed_rows_with_forced_external_decisions() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let mut connection = package_match_connection(
        tempdir.path().join("bundle.js"),
        "export function add(a,b){return a+b}",
        &[],
    );
    connection
        .execute(
            r"
            INSERT INTO package_attributions
                (module_id, module_original_name, package_name, package_version,
                 package_subpath, resolved_file, export_specifier, emission_mode,
                 status, evidence_json, rejection_reason, created_at, updated_at)
            VALUES
                (10, 'm10', 'pkg', '0.0.0', NULL, NULL, NULL,
                 'external_import', 'proposed', NULL, NULL, 'now', 'now')
            ",
            [],
        )
        .expect("insert proposed attribution");
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: true,
        package_names: Vec::new(),
        package_source_roots: Vec::new(),
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");
    let (status, emission_mode, package_version, export_specifier, rejection_reason): (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = connection
        .query_row(
            r"
            SELECT status, emission_mode, package_version, export_specifier, rejection_reason
              FROM package_attributions
             WHERE module_id = 10
            ",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .expect("proposed row should be replaced");

    assert_eq!(outcome.matched_modules, 0);
    assert_eq!(outcome.written_attributions, 1);
    assert_eq!(package_attribution_count(&connection), 1);
    assert_eq!(status, "rejected");
    assert_eq!(emission_mode, "application_source");
    assert_eq!(package_version, None);
    assert_eq!(export_specifier, None);
    assert!(rejection_reason.is_some());
    reverts_input::sqlite::load_project_bundle_from_connection(&connection, 1)
        .expect("forced external attribution should satisfy generation input contract");
}

#[test]
fn match_packages_apply_writes_source_import_package_surface() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let source_path = tempdir.path().join("bundle.js");
    fs::write(
        source_path.as_path(),
        "const client = require('undici'); export { client };",
    )
    .expect("write source fixture");
    let mut connection = Connection::open_in_memory().expect("open in-memory database");
    create_source_surface_schema(&connection);
    insert_source_surface_rows(&connection, source_path.to_string_lossy().as_ref());
    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: true,
        package_names: Vec::new(),
        package_source_roots: Vec::new(),
        materialize_package_sources: false,
    };

    let outcome = match_packages_from_connection(&mut connection, &args).expect("match should run");
    let (package_version, evidence): (String, String) = connection
        .query_row(
            "SELECT package_version, evidence_json FROM package_surfaces WHERE export_specifier = 'undici'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("package surface should be written");

    assert!(outcome.audit.is_clean());
    assert_eq!(outcome.matched_modules, 0);
    assert_eq!(outcome.matched_package_surfaces, 1);
    assert_eq!(outcome.written_attributions, 0);
    assert_eq!(outcome.written_surfaces, 1);
    assert_eq!(package_version, "2.2.1");
    assert!(evidence.contains("source_package_import_surface"));
}

#[test]
fn cli_match_packages_then_generate_project_uses_written_attribution() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let database_path = tempdir.path().join("input.db");
    let app_source_path = tempdir.path().join("app.ts");
    let package_slice_path = tempdir.path().join("pkg-add.js");
    let output_dir = tempdir.path().join("out");
    let app_source = "import { add } from 'pkg/add';\nexport const total = add(1, 2);";
    let package_slice = "export function add(a,b){return a+b}";
    fs::write(app_source_path.as_path(), app_source).expect("write app source");
    fs::write(package_slice_path.as_path(), package_slice).expect("write package slice");
    let connection = Connection::open(database_path.as_path()).expect("open fixture database");
    create_match_generate_schema(&connection);
    insert_match_generate_rows(
        &connection,
        app_source_path.to_string_lossy().as_ref(),
        package_slice_path.to_string_lossy().as_ref(),
        app_source.len() as i64,
        package_slice.len() as i64,
    );
    drop(connection);

    run([
        "match-packages".to_string(),
        "--input".to_string(),
        database_path.to_string_lossy().into_owned(),
        "--project-id".to_string(),
        "1".to_string(),
        "--apply".to_string(),
    ])
    .expect("package matching should persist attribution");
    run([
        "generate-project-v2".to_string(),
        "--input".to_string(),
        database_path.to_string_lossy().into_owned(),
        "--project-id".to_string(),
        "1".to_string(),
        "--output".to_string(),
        output_dir.to_string_lossy().into_owned(),
    ])
    .expect("generation should consume persisted attribution");

    let generated_source = fs::read_to_string(output_dir.join("modules/1-entry.ts"))
        .expect("generated entry should be written");
    assert!(generated_source.contains("import { add } from 'pkg/add';"));
    assert!(generated_source.contains("export const total = add(1, 2);"));
    assert!(!generated_source.contains("__pkg_pkg_add"));
    let connection = Connection::open(database_path).expect("reopen fixture database");
    assert_eq!(package_attribution_count(&connection), 1);
    assert_eq!(package_surface_count(&connection), 1);
}

#[test]
fn cli_extract_assets_then_generate_project_materializes_assets() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let database_path = tempdir.path().join("input.db");
    let app_source_path = tempdir.path().join("app.ts");
    let asset_path = tempdir.path().join("addon.node");
    let output_dir = tempdir.path().join("out");
    let app_source = "const native = require('/$bunfs/root/addon.node'); export { native };";
    fs::write(app_source_path.as_path(), app_source).expect("write app source");
    fs::write(asset_path.as_path(), b"native").expect("write native asset");
    let connection = Connection::open(database_path.as_path()).expect("open fixture database");
    create_match_generate_schema(&connection);
    connection
        .execute("INSERT INTO projects (id, name) VALUES (1, 'fixture')", [])
        .expect("insert project");
    connection
        .execute(
            "INSERT INTO source_files (id, file_path) VALUES (1, ?1)",
            [app_source_path.to_string_lossy().as_ref()],
        )
        .expect("insert source file");
    connection
        .execute(
            "INSERT INTO project_files (project_id, file_id) VALUES (1, 1)",
            [],
        )
        .expect("insert project file");
    connection
        .execute(
            r"
            INSERT INTO modules
                (id, file_id, original_name, semantic_name, module_category,
                 package_name, package_version, byte_start, byte_end)
            VALUES (1, 1, 'entry', 'src/index', 'application', NULL, NULL, 0, ?1)
            ",
            [app_source.len() as i64],
        )
        .expect("insert app module");
    drop(connection);

    run([
        "extract-assets".to_string(),
        "--input".to_string(),
        database_path.to_string_lossy().into_owned(),
        "--project-id".to_string(),
        "1".to_string(),
        "--apply".to_string(),
    ])
    .expect("asset extraction should persist project_assets");
    run([
        "generate-project-v2".to_string(),
        "--input".to_string(),
        database_path.to_string_lossy().into_owned(),
        "--project-id".to_string(),
        "1".to_string(),
        "--output".to_string(),
        output_dir.to_string_lossy().into_owned(),
    ])
    .expect("generation should consume persisted asset");

    let generated_source = fs::read_to_string(output_dir.join("modules/1-src/index.ts"))
        .expect("generated entry should be written");
    assert!(generated_source.contains("require('./addon.node')"));
    assert!(!generated_source.contains("/$bunfs/root/addon.node"));
    assert_eq!(
        fs::read(output_dir.join("modules/1-src/addon.node")).expect("asset should be written"),
        b"native"
    );
    assert!(
        fs::read_to_string(output_dir.join("package.json"))
            .expect("package json")
            .contains("node ./scripts/copy-assets.mjs")
    );
    let connection = Connection::open(database_path).expect("reopen fixture database");
    let stored_asset_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM project_assets", [], |row| row.get(0))
        .expect("count project assets");
    assert_eq!(stored_asset_count, 1);
}

#[test]
fn cli_extract_assets_can_materialize_bun_embedded_native_asset() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let database_path = tempdir.path().join("input.db");
    let app_source_path = tempdir.path().join("app.ts");
    let bun_executable_path = tempdir.path().join("fixture-bun");
    let output_dir = tempdir.path().join("out");
    let logical_path = "/$bunfs/root/native.node";
    let native_bytes = minimal_elf64_bytes();
    let app_source = format!("const native = require('{logical_path}'); export {{ native }};");
    let mut bun_executable = Vec::new();
    bun_executable.extend_from_slice(b"not the asset /$bunfs/root/native.node);");
    bun_executable.extend_from_slice(logical_path.as_bytes());
    bun_executable.push(0);
    bun_executable.extend_from_slice(native_bytes.as_slice());
    bun_executable.extend_from_slice(b"\0---- Bun! ----\n");
    fs::write(app_source_path.as_path(), app_source.as_str()).expect("write app source");
    fs::write(bun_executable_path.as_path(), bun_executable).expect("write bun executable");
    let connection = Connection::open(database_path.as_path()).expect("open fixture database");
    create_match_generate_schema(&connection);
    connection
        .execute("INSERT INTO projects (id, name) VALUES (1, 'fixture')", [])
        .expect("insert project");
    connection
        .execute(
            "INSERT INTO source_files (id, file_path) VALUES (1, ?1)",
            [app_source_path.to_string_lossy().as_ref()],
        )
        .expect("insert source file");
    connection
        .execute(
            "INSERT INTO project_files (project_id, file_id) VALUES (1, 1)",
            [],
        )
        .expect("insert project file");
    connection
        .execute(
            r"
            INSERT INTO modules
                (id, file_id, original_name, semantic_name, module_category,
                 package_name, package_version, byte_start, byte_end)
            VALUES (1, 1, 'entry', 'src/index', 'application', NULL, NULL, 0, ?1)
            ",
            [app_source.len() as i64],
        )
        .expect("insert app module");
    drop(connection);

    run([
        "extract-assets".to_string(),
        "--input".to_string(),
        database_path.to_string_lossy().into_owned(),
        "--project-id".to_string(),
        "1".to_string(),
        "--asset-root".to_string(),
        bun_executable_path.to_string_lossy().into_owned(),
        "--apply".to_string(),
    ])
    .expect("asset extraction should persist embedded asset");
    run([
        "generate-project-v2".to_string(),
        "--input".to_string(),
        database_path.to_string_lossy().into_owned(),
        "--project-id".to_string(),
        "1".to_string(),
        "--output".to_string(),
        output_dir.to_string_lossy().into_owned(),
    ])
    .expect("generation should consume persisted embedded asset");

    assert_eq!(
        fs::read(output_dir.join("modules/1-src/native.node"))
            .expect("embedded asset should be written"),
        native_bytes
    );
    let connection = Connection::open(database_path).expect("reopen fixture database");
    let stored_source_path: String = connection
        .query_row(
            "SELECT source_path FROM project_assets WHERE logical_path = ?1",
            [logical_path],
            |row| row.get(0),
        )
        .expect("stored embedded asset");
    assert!(PathBuf::from(stored_source_path).is_file());
}

#[test]
fn cli_extract_assets_accepts_multiple_roots_for_bun_and_vendor_assets() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let database_path = tempdir.path().join("input.db");
    let app_source_path = tempdir.path().join("app.ts");
    let bun_executable_path = tempdir.path().join("fixture-bun");
    let vendor_root = tempdir.path().join("package-root");
    let output_dir = tempdir.path().join("out");
    let native_logical_path = "/$bunfs/root/native.node";
    let native_bytes = minimal_elf64_bytes();
    let rg_path = vendor_root.join("vendor/ripgrep/x64-linux/rg");
    let app_source = format!(
        "\
        const native = require('{native_logical_path}');\n\
        const POL = {{ fileURLToPath(value) {{ return value; }} }};\n\
        const ODH = {{ join(...parts) {{ return parts.join('/'); }}, resolve(...parts) {{ return parts.join('/'); }} }};\n\
        const here = POL.fileURLToPath('file:///home/runner/work/app/src/tools/ripgrep.ts');\n\
        const base = ODH.join(here, '../');\n\
        const vendor = ODH.resolve(base, 'vendor', 'ripgrep');\n\
        const rg = ODH.resolve(vendor, 'x64-linux', 'rg');\n\
        export {{ native, rg }};"
    );
    let mut bun_executable = Vec::new();
    bun_executable.extend_from_slice(native_logical_path.as_bytes());
    bun_executable.push(0);
    bun_executable.extend_from_slice(native_bytes.as_slice());
    bun_executable.extend_from_slice(b"\0---- Bun! ----\n");
    fs::write(app_source_path.as_path(), app_source.as_str()).expect("write app source");
    fs::write(bun_executable_path.as_path(), bun_executable).expect("write bun executable");
    fs::create_dir_all(rg_path.parent().expect("rg parent")).expect("create vendor dirs");
    fs::write(rg_path.as_path(), b"rg-binary").expect("write rg");
    let connection = Connection::open(database_path.as_path()).expect("open fixture database");
    create_match_generate_schema(&connection);
    connection
        .execute("INSERT INTO projects (id, name) VALUES (1, 'fixture')", [])
        .expect("insert project");
    connection
        .execute(
            "INSERT INTO source_files (id, file_path) VALUES (1, ?1)",
            [app_source_path.to_string_lossy().as_ref()],
        )
        .expect("insert source file");
    connection
        .execute(
            "INSERT INTO project_files (project_id, file_id) VALUES (1, 1)",
            [],
        )
        .expect("insert project file");
    connection
        .execute(
            r"
            INSERT INTO modules
                (id, file_id, original_name, semantic_name, module_category,
                 package_name, package_version, byte_start, byte_end)
            VALUES (1, 1, 'entry', 'src/index', 'application', NULL, NULL, 0, ?1)
            ",
            [app_source.len() as i64],
        )
        .expect("insert app module");
    drop(connection);

    run([
        "extract-assets".to_string(),
        "--input".to_string(),
        database_path.to_string_lossy().into_owned(),
        "--project-id".to_string(),
        "1".to_string(),
        "--asset-root".to_string(),
        bun_executable_path.to_string_lossy().into_owned(),
        "--asset-root".to_string(),
        vendor_root.to_string_lossy().into_owned(),
        "--apply".to_string(),
    ])
    .expect("asset extraction should persist assets from both roots");
    run([
        "generate-project-v2".to_string(),
        "--input".to_string(),
        database_path.to_string_lossy().into_owned(),
        "--project-id".to_string(),
        "1".to_string(),
        "--output".to_string(),
        output_dir.to_string_lossy().into_owned(),
    ])
    .expect("generation should consume persisted multi-root assets");

    assert_eq!(
        fs::read(output_dir.join("modules/1-src/native.node")).expect("native asset"),
        native_bytes
    );
    assert_eq!(
        fs::read(output_dir.join("modules/1-src/vendor/ripgrep/x64-linux/rg")).expect("rg asset"),
        b"rg-binary"
    );
    let generated_source =
        fs::read_to_string(output_dir.join("modules/1-src/index.ts")).expect("generated source");
    assert!(generated_source.contains("POL.fileURLToPath(import.meta.url)"));
    assert!(!generated_source.contains("/home/runner/work/app"));
    let connection = Connection::open(database_path).expect("reopen fixture database");
    let stored_assets: i64 = connection
        .query_row("SELECT COUNT(*) FROM project_assets", [], |row| row.get(0))
        .expect("count project assets");
    assert_eq!(stored_assets, 2);
}

#[test]
fn bun_embedded_asset_extractor_reads_wasm_payload_without_trailing_bun_metadata() {
    let mut executable = Vec::new();
    executable.extend_from_slice(b"prefix");
    executable.extend_from_slice(b"/$bunfs/root/parser.wasm");
    executable.push(0);
    executable.extend_from_slice(minimal_wasm_bytes().as_slice());
    executable.extend_from_slice(b"\0---- Bun! ----\nmetadata");

    let extracted = super::commands::extract_assets::extract_bun_embedded_asset_from_bytes(
        executable.as_slice(),
        "/$bunfs/root/parser.wasm",
    )
    .expect("wasm asset should be extracted");

    assert_eq!(extracted, minimal_wasm_bytes());
}

fn minimal_elf64_bytes() -> Vec<u8> {
    let mut bytes = vec![0; 128];
    bytes[..4].copy_from_slice(b"\x7fELF");
    bytes[4] = 2;
    bytes[5] = 1;
    bytes[6] = 1;
    bytes[0x28..0x30].copy_from_slice(&(64_u64).to_le_bytes());
    bytes[0x34..0x36].copy_from_slice(&(64_u16).to_le_bytes());
    bytes[0x3a..0x3c].copy_from_slice(&(64_u16).to_le_bytes());
    bytes[0x3c..0x3e].copy_from_slice(&(1_u16).to_le_bytes());
    bytes
}

fn minimal_wasm_bytes() -> Vec<u8> {
    b"\0asm\x01\0\0\0".to_vec()
}

fn package_match(module_id: ModuleId, export_specifier: &str) -> PackageMatch {
    PackageMatch {
        module_id,
        package_name: "pkg".to_string(),
        package_version: "1.0.0".to_string(),
        export_specifier: export_specifier.to_string(),
        source_path: format!("pkg@1.0.0/{export_specifier}.js"),
        normalized_source_hash: format!("hash-{}", module_id.0),
        strategy: ModuleMatchStrategy::NormalizedSourceHash,
        function_signature_matches: 0,
        string_anchor_matches: 0,
        external_importable: true,
    }
}

fn rejected_package_ownership(
    module_id: ModuleId,
    package_name: &str,
    package_version: &str,
) -> PackageAttributionInput {
    let mut attribution = PackageAttributionInput::rejected_source(
        module_id,
        package_name,
        "matched package ownership, but the evidence does not prove a safe single external import",
    );
    attribution.package_version = Some(package_version.to_string());
    attribution
}

fn package_match_connection(
    source_path: PathBuf,
    bundled_source: &str,
    package_sources: &[(&str, &str, &str, &str)],
) -> Connection {
    fs::write(source_path.as_path(), bundled_source).expect("write source fixture");
    let connection = Connection::open_in_memory().expect("open in-memory database");
    connection
        .execute_batch(
            r"
            CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL
            );
            CREATE TABLE source_files (
                id INTEGER PRIMARY KEY,
                file_path TEXT NOT NULL
            );
            CREATE TABLE project_files (
                project_id INTEGER NOT NULL,
                file_id INTEGER NOT NULL
            );
            CREATE TABLE modules (
                id INTEGER PRIMARY KEY,
                file_id INTEGER,
                original_name TEXT NOT NULL,
                semantic_name TEXT,
                module_category TEXT,
                package_name TEXT,
                package_version TEXT,
                byte_start INTEGER,
                byte_end INTEGER
            );
            CREATE TABLE symbols (
                module_id INTEGER,
                semantic_name TEXT,
                export_name TEXT,
                original_name TEXT,
                scope_level TEXT
            );
            CREATE TABLE module_dependencies (
                module_id INTEGER,
                dependency_id INTEGER
            );
            CREATE TABLE package_source_cache (
                package_name TEXT NOT NULL,
                package_version TEXT NOT NULL,
                entry_path TEXT NOT NULL,
                source_content TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                external_importable INTEGER NOT NULL DEFAULT 1,
                external_import_policy_version INTEGER NOT NULL DEFAULT 0,
                export_specifier TEXT NOT NULL DEFAULT '',
                fetched_at TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                PRIMARY KEY (package_name, package_version, entry_path)
            );
            CREATE TABLE package_attributions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                module_id INTEGER NOT NULL,
                module_original_name TEXT NOT NULL,
                package_name TEXT NOT NULL,
                package_version TEXT NOT NULL,
                package_subpath TEXT,
                resolved_file TEXT,
                export_specifier TEXT,
                emission_mode TEXT NOT NULL,
                status TEXT NOT NULL,
                evidence_json TEXT,
                rejection_reason TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                UNIQUE (module_id)
            );
            ",
        )
        .expect("create schema");
    connection
        .execute("INSERT INTO projects (id, name) VALUES (1, 'fixture')", [])
        .expect("insert project");
    connection
        .execute(
            "INSERT INTO source_files (id, file_path) VALUES (1, ?1)",
            [source_path.to_string_lossy().as_ref()],
        )
        .expect("insert source file");
    connection
        .execute(
            "INSERT INTO project_files (project_id, file_id) VALUES (1, 1)",
            [],
        )
        .expect("insert project file");
    connection
        .execute(
            r"
            INSERT INTO modules
                (id, file_id, original_name, semantic_name, module_category,
                 package_name, package_version, byte_start, byte_end)
            VALUES (10, 1, 'm10', 'pkg/add.js', 'package', 'pkg', NULL, 0, ?1)
            ",
            [bundled_source.len() as i64],
        )
        .expect("insert module");
    for (package_name, package_version, entry_path, source) in package_sources {
        connection
            .execute(
                r"
                INSERT INTO package_source_cache
                    (package_name, package_version, entry_path, source_content,
                     content_hash, external_importable, external_import_policy_version,
                     export_specifier, fetched_at, expires_at)
                VALUES (?1, ?2, ?3, ?4, 'hash', 1, ?5, ?6, 'now', 'later')
                ",
                params![
                    package_name,
                    package_version,
                    entry_path,
                    source,
                    PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION,
                    package_export_specifier(package_name, entry_path),
                ],
            )
            .expect("insert package source");
    }
    connection
}

fn package_attribution_count(connection: &Connection) -> i64 {
    connection
        .query_row("SELECT COUNT(*) FROM package_attributions", [], |row| {
            row.get(0)
        })
        .expect("count package attributions")
}

fn package_surface_count(connection: &Connection) -> i64 {
    connection
        .query_row("SELECT COUNT(*) FROM package_surfaces", [], |row| {
            row.get(0)
        })
        .expect("count package surfaces")
}

fn create_source_surface_schema(connection: &Connection) {
    connection
        .execute_batch(
            r"
            CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL
            );
            CREATE TABLE source_files (
                id INTEGER PRIMARY KEY,
                file_path TEXT NOT NULL
            );
            CREATE TABLE project_files (
                project_id INTEGER NOT NULL,
                file_id INTEGER NOT NULL
            );
            CREATE TABLE modules (
                id INTEGER PRIMARY KEY,
                file_id INTEGER,
                original_name TEXT NOT NULL,
                semantic_name TEXT,
                module_category TEXT,
                package_name TEXT,
                package_version TEXT,
                byte_start INTEGER,
                byte_end INTEGER
            );
            CREATE TABLE symbols (
                module_id INTEGER,
                semantic_name TEXT,
                export_name TEXT,
                original_name TEXT,
                scope_level TEXT
            );
            CREATE TABLE module_dependencies (
                module_id INTEGER,
                dependency_id INTEGER
            );
            CREATE TABLE package_source_cache (
                package_name TEXT NOT NULL,
                package_version TEXT NOT NULL,
                entry_path TEXT NOT NULL,
                source_content TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                external_importable INTEGER NOT NULL DEFAULT 1,
                external_import_policy_version INTEGER NOT NULL DEFAULT 0,
                export_specifier TEXT NOT NULL DEFAULT '',
                fetched_at TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                PRIMARY KEY (package_name, package_version, entry_path)
            );
            CREATE TABLE package_attributions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                module_id INTEGER NOT NULL,
                module_original_name TEXT NOT NULL,
                package_name TEXT NOT NULL,
                package_version TEXT NOT NULL,
                package_subpath TEXT,
                resolved_file TEXT,
                export_specifier TEXT,
                emission_mode TEXT NOT NULL,
                status TEXT NOT NULL,
                evidence_json TEXT,
                rejection_reason TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                UNIQUE (module_id)
            );
            ",
        )
        .expect("create source surface schema");
}

fn insert_source_surface_rows(connection: &Connection, source_path: &str) {
    let app_source = fs::read_to_string(source_path).expect("source fixture should exist");
    connection
        .execute("INSERT INTO projects (id, name) VALUES (1, 'fixture')", [])
        .expect("insert project");
    connection
        .execute(
            "INSERT INTO source_files (id, file_path) VALUES (1, ?1)",
            [source_path],
        )
        .expect("insert source file");
    connection
        .execute(
            "INSERT INTO project_files (project_id, file_id) VALUES (1, 1)",
            [],
        )
        .expect("insert project file");
    connection
        .execute(
            r"
            INSERT INTO modules
                (id, file_id, original_name, semantic_name, module_category,
                 package_name, package_version, byte_start, byte_end)
            VALUES (1, 1, 'entry', 'entry', 'application', NULL, NULL, 0, ?1)
            ",
            [app_source.len() as i64],
        )
        .expect("insert app module");
    connection
        .execute(
            r"
            INSERT INTO package_source_cache
                (package_name, package_version, entry_path, source_content,
                 content_hash, external_importable, external_import_policy_version,
                 export_specifier, fetched_at, expires_at)
            VALUES
                ('undici', '2.2.1', 'wrapper.mjs', 'export default {};',
                 'hash', 1, ?1, 'undici/wrapper.mjs', 'now', 'later')
            ",
            [PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION],
        )
        .expect("insert package source");
}

fn create_match_generate_schema(connection: &Connection) {
    connection
        .execute_batch(
            r"
            CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL
            );
            CREATE TABLE source_files (
                id INTEGER PRIMARY KEY,
                file_path TEXT NOT NULL
            );
            CREATE TABLE project_files (
                project_id INTEGER NOT NULL,
                file_id INTEGER NOT NULL
            );
            CREATE TABLE modules (
                id INTEGER PRIMARY KEY,
                file_id INTEGER,
                original_name TEXT NOT NULL,
                semantic_name TEXT,
                module_category TEXT,
                package_name TEXT,
                package_version TEXT,
                byte_start INTEGER,
                byte_end INTEGER
            );
            CREATE TABLE symbols (
                module_id INTEGER,
                semantic_name TEXT,
                export_name TEXT,
                original_name TEXT,
                scope_level TEXT
            );
            CREATE TABLE module_dependencies (
                module_id INTEGER,
                dependency_id INTEGER
            );
            CREATE TABLE package_source_cache (
                package_name TEXT NOT NULL,
                package_version TEXT NOT NULL,
                entry_path TEXT NOT NULL,
                source_content TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                external_importable INTEGER NOT NULL DEFAULT 1,
                external_import_policy_version INTEGER NOT NULL DEFAULT 0,
                export_specifier TEXT NOT NULL DEFAULT '',
                fetched_at TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                PRIMARY KEY (package_name, package_version, entry_path)
            );
            CREATE TABLE package_attributions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                module_id INTEGER NOT NULL,
                module_original_name TEXT NOT NULL,
                package_name TEXT NOT NULL,
                package_version TEXT NOT NULL,
                package_subpath TEXT,
                resolved_file TEXT,
                export_specifier TEXT,
                emission_mode TEXT NOT NULL,
                status TEXT NOT NULL,
                evidence_json TEXT,
                rejection_reason TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                UNIQUE (module_id)
            );
            ",
        )
        .expect("create match/generate schema");
}

fn insert_match_generate_rows(
    connection: &Connection,
    app_source_path: &str,
    package_slice_path: &str,
    app_len: i64,
    package_len: i64,
) {
    connection
        .execute("INSERT INTO projects (id, name) VALUES (1, 'fixture')", [])
        .expect("insert project");
    connection
        .execute(
            "INSERT INTO source_files (id, file_path) VALUES (1, ?1), (2, ?2)",
            params![app_source_path, package_slice_path],
        )
        .expect("insert source files");
    connection
        .execute(
            "INSERT INTO project_files (project_id, file_id) VALUES (1, 1), (1, 2)",
            [],
        )
        .expect("insert project files");
    connection
        .execute(
            r"
            INSERT INTO modules
                (id, file_id, original_name, semantic_name, module_category,
                 package_name, package_version, byte_start, byte_end)
            VALUES
                (1, 1, 'entry', 'entry', 'application', NULL, NULL, 0, ?1),
                (10, 2, 'pkg_add', 'pkg/add', 'package', 'pkg', NULL, 0, ?2)
            ",
            params![app_len, package_len],
        )
        .expect("insert modules");
    connection
        .execute(
            r"
            INSERT INTO package_source_cache
                (package_name, package_version, entry_path, source_content,
                 content_hash, external_importable, external_import_policy_version,
                 export_specifier, fetched_at, expires_at)
            VALUES
                ('pkg', '1.2.3', 'add', 'export function add(a, b) { return a + b; }',
                 'hash', 1, ?1, 'pkg/add', 'now', 'later')
            ",
            [PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION],
        )
        .expect("insert package source");
}
