use std::collections::{BTreeMap, BTreeSet};

use crate::{
    BestVersionMatch, CascadeMatchReport, CascadeOwnershipMatch, ConcretePackageSourcePath,
    ExternalImportProofScratch, ExternalImportSourceIndex, ModuleMatchStrategy,
    PACKAGE_SOURCE_FINGERPRINT_MAX_BYTES, PackageMatch, PackageModuleSourceQuality, PackageSource,
    VersionedPackageMatchReport, VersionedPackageMatcher, match_packages_with_pipeline,
    match_structural_bags, match_structural_bags_with_excluded_modules,
    ownership::{cascade, exact_hint},
    package_import_names_from_sources, package_module_source_quality,
    package_source_normalized_hash, package_source_normalized_hashes,
    package_source_public_export_proofs, resolve_external_import_target,
    same_package_cross_version_source_external_import_target,
};
use reverts_graph::FunctionExtractor;
use reverts_input::{
    AttributionConfidence, InputRows, ModuleDependencyInput, ModuleDependencyTarget, ModuleInput,
    PackageAttributionInput, ProjectInput, SourceFileInput, SourceSpan,
};
use reverts_ir::{AxisKind, MatchTier, ModuleId};
use reverts_observe::FindingCode;

fn rows_with_package_source(source: &str) -> InputRows {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "bundle.js",
        Some(source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(ModuleId(10), "m10", "pkg/module.ts", "pkg", None).with_source_file(1),
    );
    rows
}

fn rows_with_package_source_at_version(source: &str, version: &str) -> InputRows {
    let mut rows = rows_with_package_source(source);
    rows.modules[0].package_version = Some(version.to_string());
    rows
}

fn cascade_confidence(tier: MatchTier) -> AttributionConfidence {
    AttributionConfidence {
        tier,
        matched_axes: vec![AxisKind::StructuralAnchor],
        matched_alternate: None,
        top_score: tier.weight() as f64,
        runner_up_score: 0.0,
        margin: 1.0,
    }
}

#[test]
fn pipeline_does_not_externalize_empty_source_scope_without_proof() {
    let rows = rows_with_package_source("export function add(a,b){return a+b}");

    let report = match_packages_with_pipeline(&rows, &[], None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 0);
    assert_eq!(report.package_report.attributions.len(), 0);
    assert!(report.function_attributions.is_empty());
    assert_eq!(report.function_ownership_matches, 0);
}

#[test]
fn pipeline_recognizes_anonymous_bundle_package_ownership_without_externalizing() {
    let source = "function add(a,b){return a+b;} function sub(a,b){return a-b;}";
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "bundle.js",
        Some(source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(
            ModuleId(20),
            "20-esbuild-anon",
            "modules/20-esbuild-anon.ts",
        )
        .with_source_file(1),
    );
    let package_sources = [PackageSource::external(
        "pkg", "1.0.0", "pkg", "index.js", source,
    )];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 1);
    let package_match = &report.package_report.matches[0];
    assert_eq!(package_match.module_id, ModuleId(20));
    assert_eq!(package_match.package_name, "pkg");
    assert!(
        !package_match.external_importable,
        "anonymous bundle ownership must not become a public import"
    );
    assert!(report.package_report.attributions.is_empty());
    assert!(report.function_attributions.is_empty());
    assert!(report.function_ownership_matches > 0);
}

#[test]
fn pipeline_recognizes_anonymous_bundle_package_by_function_axes() {
    let source = "function a(x){if(x){return true;}return false;} function b(y){return y ? 1 : 0;}";
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "bundle.js",
        Some(source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(
            ModuleId(20),
            "20-esbuild-anon",
            "modules/20-esbuild-anon.ts",
        )
        .with_source_file(1),
    );
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg",
            "index.js",
            "function first(value){if(value){return true;}return false;} function second(input){return input ? 1 : 0;}",
        ),
        PackageSource::external(
            "other",
            "1.0.0",
            "other",
            "index.js",
            "function unrelated(){return 'different';}",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 1);
    let package_match = &report.package_report.matches[0];
    assert_eq!(package_match.module_id, ModuleId(20));
    assert_eq!(package_match.package_name, "pkg");
    assert_eq!(
        package_match.strategy,
        ModuleMatchStrategy::AggregateFunctionSignatureAndStringAnchors
    );
    assert!(!package_match.external_importable);
    assert!(report.package_report.attributions.is_empty());
}

#[test]
fn pipeline_promotes_anonymous_module_by_package_source_graph_neighborhood() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "a.js",
        Some("import './b.js'; export const a = 1;".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "b.js",
        Some("const unrelated = 42;".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        3,
        "c.js",
        Some("export const c = 3;".to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(10),
            "a",
            "node_modules/pkg/a.js",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(20), "anon-b", "modules/anon-b.js").with_source_file(2),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(30),
            "c",
            "node_modules/pkg/c.js",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(3),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(10),
        target: ModuleDependencyTarget::Module(ModuleId(20)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(20),
        target: ModuleDependencyTarget::Module(ModuleId(30)),
    });
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/a",
            "a.js",
            "import './b.js'; export const a = 1;",
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/b",
            "b.js",
            "import './c.js'; export const b = 2;",
        ),
        PackageSource::external("pkg", "1.0.0", "pkg/c", "c.js", "export const c = 3;"),
        PackageSource::external(
            "other",
            "1.0.0",
            "other",
            "index.js",
            "export const unrelated = true;",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    let graph_match = report
        .package_report
        .matches
        .iter()
        .find(|package_match| package_match.module_id == ModuleId(20))
        .expect("anonymous module promoted by package source graph");
    assert_eq!(graph_match.package_name, "pkg");
    assert_eq!(graph_match.package_version, "1.0.0");
    assert_eq!(
        graph_match.strategy,
        ModuleMatchStrategy::PackageGraphNeighborhoodOwnership
    );
    assert_eq!(graph_match.function_signature_matches, 2);
    assert_eq!(graph_match.string_anchor_matches, 2);
    assert!(
        !graph_match.external_importable,
        "graph-neighborhood ownership is not a public import proof"
    );
}

#[test]
fn pipeline_promotes_anonymous_module_by_owned_dependency_neighborhood() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "a.js",
        Some("export const a = 1;".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "helper.js",
        Some("exports.helper = Object;".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        3,
        "c.js",
        Some("export const c = 3;".to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(10),
            "a",
            "node_modules/pkg/a.js",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(20), "anon-helper", "modules/helper.js")
            .with_source_file(2),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(30),
            "c",
            "node_modules/pkg/c.js",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(3),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(10),
        target: ModuleDependencyTarget::Module(ModuleId(20)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(30),
        target: ModuleDependencyTarget::Module(ModuleId(20)),
    });
    let package_sources = [
        PackageSource::external("pkg", "1.0.0", "pkg/a", "a.js", "export const a = 1;"),
        PackageSource::external("pkg", "1.0.0", "pkg/c", "c.js", "export const c = 3;"),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    let dependency_match = report
        .package_report
        .matches
        .iter()
        .find(|package_match| package_match.module_id == ModuleId(20))
        .expect("anonymous module promoted by owned dependency neighborhood");
    assert_eq!(dependency_match.package_name, "pkg");
    assert_eq!(
        dependency_match.strategy,
        ModuleMatchStrategy::DependencyClosureOwnership
    );
    assert!(
        dependency_match
            .source_path
            .starts_with("anonymous-dependency-neighborhood:pkg@1.0.0:")
    );
    assert_eq!(dependency_match.function_signature_matches, 2);
    assert_eq!(dependency_match.string_anchor_matches, 2);
    assert!(!dependency_match.external_importable);
}

#[test]
fn pipeline_promotes_tiny_outgoing_only_dependency_neighborhood() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "barrel.js",
        Some("var barrel = E(() => { helper(); });".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "helper.js",
        Some("export function helper(){return 'helper-anchor';}".to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(10), "barrel", "modules/barrel.js").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(11),
            "helper",
            "pkg/helper.js",
            "pkg",
            Some("1.2.3".to_string()),
        )
        .with_source_file(2),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(10),
        target: ModuleDependencyTarget::Module(ModuleId(11)),
    });
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/helper",
        "helper.js",
        "export function helper(){return 'helper-anchor';}",
    )];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    let barrel_match = report
        .package_report
        .matches
        .iter()
        .find(|package_match| package_match.module_id == ModuleId(10))
        .expect("tiny outgoing-only wrapper should be promoted");
    assert_eq!(barrel_match.package_name, "pkg");
    assert_eq!(
        barrel_match.strategy,
        ModuleMatchStrategy::DependencyClosureOwnership
    );
    assert!(barrel_match.source_path.contains("incoming=0"));
    assert!(barrel_match.source_path.contains("outgoing=1"));
    assert!(!barrel_match.external_importable);
}

#[test]
fn pipeline_infers_ownership_from_incoming_package_even_with_external_outgoing_dependency() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    for (id, path, source) in [
        (
            1,
            "consumer.js",
            "export const consumer = 'consumer-anchor';",
        ),
        (2, "helper.js", "var helper = E(() => dep());"),
        (3, "dep.js", "export const dep = 'dep-anchor';"),
    ] {
        rows.source_files
            .push(SourceFileInput::new(id, path, Some(source.to_string())));
    }
    rows.modules.push(
        ModuleInput::package(
            ModuleId(10),
            "consumer",
            "pkg/consumer.js",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(20), "helper", "modules/helper.js").with_source_file(2),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(30),
            "dep",
            "dep/index.js",
            "dep",
            Some("1.0.0".to_string()),
        )
        .with_source_file(3),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(10),
        target: ModuleDependencyTarget::Module(ModuleId(20)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(20),
        target: ModuleDependencyTarget::Module(ModuleId(30)),
    });
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/consumer",
            "consumer.js",
            "export const consumer = 'consumer-anchor';",
        ),
        PackageSource::external(
            "dep",
            "1.0.0",
            "dep",
            "index.js",
            "export const dep = 'dep-anchor';",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    let helper_match = report
        .package_report
        .matches
        .iter()
        .find(|package_match| package_match.module_id == ModuleId(20))
        .expect("incoming package should own its private helper");
    assert_eq!(helper_match.package_name, "pkg");
    assert_eq!(
        helper_match.strategy,
        ModuleMatchStrategy::DependencyClosureOwnership
    );
    assert!(helper_match.source_path.contains("incoming=1"));
}

#[test]
fn pipeline_promotes_anonymous_module_by_rare_package_anchors() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "anon.js",
        Some(
            r#"
            Object.defineProperty(exports, "__esModule", { value: true });
            exports.fromRemoteMetadata = void 0;
            const log = "@scope/pkg-runtime";
            "#
            .to_string(),
        ),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(10), "anon", "modules/anon.js").with_source_file(1),
    );
    let mut package_sources = vec![
        PackageSource::source_only(
            "@scope/pkg",
            "1.0.0",
            "@scope/pkg/runtime",
            "runtime.js",
            r#"
            export const fromRemoteMetadata = () => "@scope/pkg-runtime";
            "#,
        ),
        PackageSource::source_only(
            "other",
            "1.0.0",
            "other/index",
            "index.js",
            "export const unrelated = 'other-runtime';",
        ),
    ];
    for index in 0..32 {
        package_sources.push(PackageSource::source_only(
            format!("filler-{index}"),
            "1.0.0",
            format!("filler-{index}"),
            "index.js",
            format!("export const filler{index} = 'filler-{index}';"),
        ));
    }

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    let anchor_match = report
        .package_report
        .matches
        .iter()
        .find(|package_match| package_match.module_id == ModuleId(10))
        .expect("rare anchors should promote package ownership");
    assert_eq!(anchor_match.package_name, "@scope/pkg");
    assert_eq!(
        anchor_match.strategy,
        ModuleMatchStrategy::AggregateStringAnchorSimilarity
    );
    assert_eq!(anchor_match.string_anchor_matches, 2);
    assert!(!anchor_match.external_importable);
}

#[test]
fn pipeline_promotes_short_anonymous_module_by_dependency_cluster() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    for (id, source) in [
        (1, "export const a = 1;"),
        (2, "export const b = 2;"),
        (3, "export const c = 3;"),
        (4, "exports.tiny = 4;"),
        (
            5,
            "exports.large = \"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\";",
        ),
    ] {
        rows.source_files.push(SourceFileInput::new(
            id,
            format!("{id}.js"),
            Some(source.to_string()),
        ));
    }
    for (id, name) in [(10, "a"), (20, "b"), (30, "c")] {
        rows.modules.push(
            ModuleInput::package(
                ModuleId(id),
                name,
                format!("node_modules/pkg/{name}.js"),
                "pkg",
                Some("1.0.0".to_string()),
            )
            .with_source_file(id / 10),
        );
    }
    rows.modules.push(
        ModuleInput::application(ModuleId(40), "tiny", "modules/tiny.js").with_source_file(4),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(50), "large", "modules/large.js").with_source_file(5),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(10),
        target: ModuleDependencyTarget::Module(ModuleId(20)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(20),
        target: ModuleDependencyTarget::Module(ModuleId(30)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(40),
        target: ModuleDependencyTarget::Module(ModuleId(20)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(40),
        target: ModuleDependencyTarget::Module(ModuleId(50)),
    });
    let package_sources = [
        PackageSource::external("pkg", "1.0.0", "pkg/a", "a.js", "export const a = 1;"),
        PackageSource::external("pkg", "1.0.0", "pkg/b", "b.js", "export const b = 2;"),
        PackageSource::external("pkg", "1.0.0", "pkg/c", "c.js", "export const c = 3;"),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    let cluster_match = report
        .package_report
        .matches
        .iter()
        .find(|package_match| package_match.module_id == ModuleId(40))
        .expect("short anonymous module promoted by dependency cluster");
    assert_eq!(cluster_match.package_name, "pkg");
    assert!(
        cluster_match
            .source_path
            .starts_with("anonymous-dependency-cluster:pkg@1.0.0:")
    );
    assert!(!cluster_match.external_importable);
}

#[test]
fn package_source_normalized_hashes_include_stable_pass_alternates() {
    let source_with_boundary = "function add(a,b){return a+b;}\nexports.add = add;";
    let source_without_boundary = "function add(a,b){return a+b;}";
    let base_hash = package_source_normalized_hash("pkg@1.0.0/lib/add.js", source_with_boundary)
        .expect("source with boundary should normalize");
    let stripped_hash =
        package_source_normalized_hash("pkg@1.0.0/lib/add.js", source_without_boundary)
            .expect("source without boundary should normalize");

    let hashes = package_source_normalized_hashes("pkg@1.0.0/lib/add.js", source_with_boundary);

    assert!(hashes.contains(&base_hash));
    assert!(
        hashes.contains(&stripped_hash),
        "stable normalization alternates should prove export-boundary-equivalent source"
    );
}

#[test]
fn package_module_source_quality_rejects_unparseable_span() {
    let module = ModuleInput::package(
        ModuleId(10),
        "m10",
        "modules/10-rxjs/operators/sample.ts",
        "rxjs",
        Some("7.8.2".to_string()),
    );

    let quality = package_module_source_quality(
        &module,
        "bundle.js",
        "lts.allowAbsoluteUrls !== void 0) K.allowAbsoluteU",
    );

    assert_eq!(quality, PackageModuleSourceQuality::Invalid);
}

#[test]
fn package_module_source_quality_marks_parseable_missing_hint_as_weak() {
    let module = ModuleInput::package(
        ModuleId(10),
        "m10",
        "modules/10-rxjs/operators/sample.ts",
        "rxjs",
        Some("7.8.2".to_string()),
    );

    let quality =
        package_module_source_quality(&module, "bundle.js", "function unrelated(){return 1;}");

    assert_eq!(quality, PackageModuleSourceQuality::Weak);
}

#[test]
fn package_module_source_quality_trusts_parseable_hint_token() {
    let module = ModuleInput::package(
        ModuleId(10),
        "m10",
        "modules/10-rxjs/operators/sample.ts",
        "rxjs",
        Some("7.8.2".to_string()),
    );

    let quality =
        package_module_source_quality(&module, "bundle.js", "function sample(){return 1;}");

    assert_eq!(quality, PackageModuleSourceQuality::Trusted);
}

#[test]
fn versioned_matcher_skips_weak_path_hint_for_exact_matching() {
    let mut rows = rows_with_package_source("function unrelated(){return 1;}");
    rows.modules[0].semantic_path = "pkg/sample.ts".to_string();
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/sample",
        "sample.js",
        "function unrelated(){return 1;}",
    )];

    let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

    assert!(report.audit.is_clean());
    assert!(report.matches.is_empty());
    assert!(report.attributions.is_empty());
}

#[test]
fn exact_match_uses_normalized_source_before_accepting_attribution() {
    let rows = rows_with_package_source_at_version("export function add(a,b){return a+b}", "1.2.3");
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/add",
        "add.js",
        "export function add(a, b) {\n  return a + b;\n}",
    )];

    let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

    assert!(report.audit.is_clean());
    assert_eq!(report.attributions.len(), 1);
    assert_eq!(report.attributions[0].package_name, "pkg");
    assert_eq!(
        report.attributions[0].package_version.as_deref(),
        Some("1.2.3")
    );
    assert_eq!(
        report.attributions[0].export_specifier.as_deref(),
        Some("pkg/add")
    );
    assert_eq!(report.attributions[0].subpath.as_deref(), Some("add"));
}

#[test]
fn external_import_proof_scratch_reuses_source_and_graph_evidence() {
    let source = "export function add(a, b) { return a + b; }";
    let rows = rows_with_package_source_at_version(source, "1.0.0");
    let package_sources = [PackageSource::external(
        "pkg",
        "1.0.0",
        "pkg/add",
        "pkg@1.0.0/lib/add.js",
        source,
    )];
    let index = ExternalImportSourceIndex::build(&package_sources);
    let cache = ExternalImportProofScratch::default();

    let first_module_fingerprint = cache
        .module_fingerprint(
            &rows.modules[0],
            rows.modules[0].semantic_path.as_str(),
            source,
        )
        .expect("module fingerprint");
    let second_module_fingerprint = cache
        .module_fingerprint(
            &rows.modules[0],
            rows.modules[0].semantic_path.as_str(),
            source,
        )
        .expect("module fingerprint");
    assert_eq!(
        first_module_fingerprint.normalized_source_hashes,
        second_module_fingerprint.normalized_source_hashes
    );
    assert_eq!(cache.module_fingerprints.borrow().len(), 1);

    assert_eq!(
        cache
            .source_fingerprints_for_version(&index, "pkg", "1.0.0")
            .len(),
        1
    );
    assert_eq!(
        cache
            .source_fingerprints_for_version(&index, "pkg", "1.0.0")
            .len(),
        1
    );
    assert_eq!(cache.source_fingerprints_by_version.borrow().len(), 1);

    let package_fingerprints = cache.source_fingerprints_for_package(&index, "pkg");
    assert_eq!(package_fingerprints.len(), 1);
    assert_eq!(
        cache.source_fingerprints_for_package(&index, "pkg").len(),
        1
    );
    assert_eq!(cache.source_fingerprints_by_package.borrow().len(), 1);

    let concrete_sources_by_module = BTreeMap::new();
    let first_graph = cache.dependency_graph_evidence(
        &rows,
        rows.modules[0].id,
        package_fingerprints[0].source,
        &index,
        &concrete_sources_by_module,
    );
    let second_graph = cache.dependency_graph_evidence(
        &rows,
        rows.modules[0].id,
        package_fingerprints[0].source,
        &index,
        &concrete_sources_by_module,
    );
    assert_eq!(first_graph.matched_edges, second_graph.matched_edges);
    assert_eq!(first_graph.known_edges, second_graph.known_edges);
    assert_eq!(cache.dependency_graph_evidence.borrow().len(), 1);

    let mut unrelated_concrete_sources_by_module = BTreeMap::new();
    unrelated_concrete_sources_by_module.insert(
        ModuleId(99),
        ConcretePackageSourcePath {
            package_name: "other-pkg".to_string(),
            package_version: "1.0.0".to_string(),
            source_path: "other-pkg@1.0.0/index.js".to_string(),
        },
    );
    let unrelated_graph = cache.dependency_graph_evidence(
        &rows,
        rows.modules[0].id,
        package_fingerprints[0].source,
        &index,
        &unrelated_concrete_sources_by_module,
    );
    assert_eq!(first_graph.matched_edges, unrelated_graph.matched_edges);
    assert_eq!(first_graph.known_edges, unrelated_graph.known_edges);
    assert_eq!(
        cache.dependency_graph_evidence.borrow().len(),
        1,
        "unrelated concrete modules should reuse the same graph-evidence cache entry"
    );
}

#[test]
fn versioned_matcher_uses_module_level_normalization_alternates() {
    let rows = rows_with_package_source_at_version("export function add(a,b){return a+b}", "1.2.3");
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/add",
        "add.js",
        "function add(a, b) {\n  return a + b;\n}",
    )];

    let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

    assert!(report.audit.is_clean());
    assert_eq!(report.attributions.len(), 1);
    assert_eq!(
        report.matches[0].strategy,
        ModuleMatchStrategy::NormalizedSourceHash,
        "export-boundary normalization should produce a source hash match"
    );
    let selected = report
        .version_matches
        .iter()
        .find_map(|decision| match decision {
            BestVersionMatch::Selected { score, .. } => Some(score),
            _ => None,
        })
        .expect("exact version should be selected");
    assert_eq!(selected.source_hash_matches, 1);
}

#[test]
fn versioned_matcher_matches_cjs_and_esm_export_boundaries() {
    let rows = rows_with_package_source_at_version(
        "function add(a,b){return a+b}\nexports.add = add;",
        "1.2.3",
    );
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/add",
        "add.js",
        "export function add(a, b) {\n  return a + b;\n}",
    )];

    let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

    assert!(report.audit.is_clean());
    assert_eq!(report.attributions.len(), 1);
    assert_eq!(
        report.matches[0].strategy,
        ModuleMatchStrategy::NormalizedSourceHash,
        "CommonJS export footer stripping should recover a source hash match"
    );
}

#[test]
fn versioned_matcher_matches_commonjs_define_property_reexport() {
    let rows = rows_with_package_source_at_version(
        r#"function add(a,b){return a+b}
Object.defineProperty(exports, "add", { enumerable: true, get: function () { return add; } });"#,
        "1.2.3",
    );
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/add",
        "add.js",
        "export function add(a, b) {\n  return a + b;\n}",
    )];

    let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

    assert!(report.audit.is_clean());
    assert_eq!(report.attributions.len(), 1);
    assert_eq!(
        report.matches[0].strategy,
        ModuleMatchStrategy::NormalizedSourceHash
    );
}

#[test]
fn versioned_matcher_externalizes_exact_json_source_with_resolved_file() {
    let source = "export default {\"aliceblue\":\"#f0f8ff\"};\n";
    let rows = rows_with_package_source_at_version(source, "1.0.0");
    let package_sources = [PackageSource::external(
        "pkg",
        "1.0.0",
        "pkg",
        "pkg@1.0.0/data.json",
        source,
    )];

    let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

    assert!(report.audit.is_clean());
    assert_eq!(report.attributions.len(), 1);
    assert_eq!(
        report.matches[0].strategy,
        ModuleMatchStrategy::NormalizedSourceHash
    );
    assert!(report.matches[0].external_importable);
    assert_eq!(
        report.attributions[0].resolved_file.as_deref(),
        Some("pkg@1.0.0/data.json")
    );
}

#[test]
fn exact_hint_promotion_does_not_externalize_without_source_match() {
    let rows = rows_with_package_source_at_version("export const unrelated = 42;", "1.0.0");
    let package_sources = [PackageSource::external(
        "pkg",
        "1.0.0",
        "pkg/other",
        "pkg@1.0.0/index.js",
        "export const packageRoot = 1;",
    )];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 1);
    assert_eq!(
        report.package_report.matches[0].strategy,
        ModuleMatchStrategy::DependencyClosureOwnership
    );
    assert!(!report.package_report.matches[0].external_importable);
    assert!(report.package_report.attributions.is_empty());
}

#[test]
fn versioned_matcher_uses_package_aggregate_ownership_when_sources_are_split() {
    let rows = rows_with_package_source_at_version(
        r#"
        function one(){return "alpha-anchor";}
        function two(){return "beta-anchor";}
        function three(){return "gamma-anchor";}
        "#,
        "1.2.3",
    );
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/one",
            "one.js",
            r#"function one(){return "alpha-anchor";}"#,
        ),
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/two",
            "two.js",
            r#"function two(){return "beta-anchor";}"#,
        ),
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/three",
            "three.js",
            r#"function three(){return "gamma-anchor";}"#,
        ),
    ];

    let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

    assert!(report.audit.is_clean());
    assert!(
        report.attributions.is_empty(),
        "aggregate package ownership must not emit a single external import"
    );
    assert_eq!(report.matches.len(), 1);
    assert_eq!(
        report.matches[0].strategy,
        ModuleMatchStrategy::AggregateFunctionSignatureAndStringAnchors
    );
    assert!(!report.matches[0].external_importable);
    assert!(report.matches[0].function_signature_matches >= 3);
}

#[test]
fn structural_bag_matches_weak_minified_aggregate_without_external_import() {
    let mut rows = rows_with_package_source(
        r#"
        function a(x){if(x){return true;}return false;}
        function b(y){if(y){return true;}return false;}
        "#,
    );
    rows.modules[0].semantic_path = "pkg/not-present-in-source.js".to_string();
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/first",
            "first.js",
            "function first(value){if(value){return true;}return false;}",
        ),
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/second",
            "second.js",
            "function second(input){if(input){return true;}return false;}",
        ),
    ];

    let report = match_structural_bags(&rows, &package_sources, None);

    assert!(report.audit.is_clean());
    assert_eq!(report.matches.len(), 1);
    assert_eq!(
        report.matches[0].strategy,
        ModuleMatchStrategy::AggregateStructuralBagSimilarity
    );
    assert_eq!(report.matches[0].package_version, "1.2.3");
    assert!(!report.matches[0].external_importable);
    assert_eq!(report.matches[0].function_signature_matches, 2);
    assert!(
        report.matches[0].string_anchor_matches >= 2,
        "structural bag should count strong structural axes as evidence"
    );
}

#[test]
fn structural_bag_rejects_equal_versions_without_version_hint() {
    let rows = rows_with_package_source(
        r#"
        function a(x){if(x){return true;}return false;}
        function b(y){if(y){return true;}return false;}
        "#,
    );
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/first",
            "first.js",
            "function first(value){if(value){return true;}return false;}",
        ),
        PackageSource::external(
            "pkg",
            "2.0.0",
            "pkg/first",
            "first.js",
            "function first(value){if(value){return true;}return false;}",
        ),
    ];

    let report = match_structural_bags(&rows, &package_sources, None);

    assert!(report.audit.is_clean());
    assert!(
        report.matches.is_empty(),
        "equal structural evidence across versions must not infer a version"
    );
}

#[test]
fn structural_bag_uses_exact_module_version_hint_for_equal_versions() {
    let mut rows = rows_with_package_source(
        r#"
        function a(x){if(x){return true;}return false;}
        function b(y){if(y){return true;}return false;}
        "#,
    );
    rows.modules[0].package_version = Some("1.0.0".to_string());
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/first",
            "first.js",
            "function first(value){if(value){return true;}return false;}",
        ),
        PackageSource::external(
            "pkg",
            "2.0.0",
            "pkg/first",
            "first.js",
            "function first(value){if(value){return true;}return false;}",
        ),
    ];

    let report = match_structural_bags(&rows, &package_sources, None);

    assert!(report.audit.is_clean());
    assert_eq!(report.matches.len(), 1);
    assert_eq!(report.matches[0].package_version, "1.0.0");
    assert_eq!(
        report.matches[0].strategy,
        ModuleMatchStrategy::AggregateStructuralBagSimilarity
    );
}

#[test]
fn structural_bag_skips_modules_already_matched_by_stronger_strategy() {
    let source = r#"
        function a(x){if(x){return true;}return false;}
        function b(y){if(y){return true;}return false;}
        "#;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "bundle-one.js",
        Some(source.to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "bundle-two.js",
        Some(source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(ModuleId(10), "m10", "pkg/one.js", "pkg", None).with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(ModuleId(11), "m11", "pkg/two.js", "pkg", None).with_source_file(2),
    );
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/first",
            "first.js",
            "function first(value){if(value){return true;}return false;}",
        ),
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/second",
            "second.js",
            "function second(input){if(input){return true;}return false;}",
        ),
    ];

    let report = match_structural_bags_with_excluded_modules(
        &rows,
        &package_sources,
        None,
        &BTreeSet::from([ModuleId(10)]),
    );

    assert!(report.audit.is_clean());
    assert_eq!(report.matches.len(), 1);
    assert_eq!(report.matches[0].module_id, ModuleId(11));
}

#[test]
fn pipeline_externalizes_structural_bag_ownership() {
    let mut rows = rows_with_package_source(
        r#"
        function a(x){if(x){return true;}return false;}
        function b(y){if(y){return true;}return false;}
        "#,
    );
    rows.modules[0].semantic_path = "pkg/not-present-in-source.js".to_string();
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/first",
            "first.js",
            "function first(value){if(value){return true;}return false;}",
        ),
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/second",
            "second.js",
            "function second(input){if(input){return true;}return false;}",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(
        report
            .package_report
            .matches
            .iter()
            .filter(|package_match| package_match.strategy
                == ModuleMatchStrategy::AggregateStructuralBagSimilarity)
            .count(),
        1
    );
    let package_match = report
        .package_report
        .matches
        .iter()
        .find(|package_match| package_match.module_id == ModuleId(10))
        .expect("structural ownership should be promoted into package report");
    assert_eq!(
        package_match.strategy,
        ModuleMatchStrategy::AggregateStructuralBagSimilarity
    );
    assert!(
        package_match
            .source_path
            .contains("structural-bag:pkg@1.2.3")
    );
    assert!(!package_match.external_importable);
    assert!(
        report.package_report.attributions.is_empty(),
        "ownership-only structural evidence must not emit an unproven external import"
    );
}

#[test]
fn pipeline_keeps_weak_full_cascade_coverage_source_only() {
    let source = "function initPackage(){return helper(1);}";
    let rows = rows_with_package_source_at_version(source, "1.2.3");
    let fingerprints = FunctionExtractor::fingerprint(ModuleId(10), source);
    assert_eq!(fingerprints.len(), 1);
    let function_span = fingerprints[0].id.span;
    let cascade_report = CascadeMatchReport {
        attributions: Vec::new(),
        ownership_matches: vec![CascadeOwnershipMatch {
            module_id: ModuleId(10),
            package_name: "pkg".to_string(),
            package_version: "1.2.3".to_string(),
            export_specifier: "pkg/init".to_string(),
            function_span,
            confidence: cascade_confidence(MatchTier::StructuralOnly),
            external_importable: true,
        }],
        audit: Default::default(),
    };
    let mut report = VersionedPackageMatchReport {
        attributions: Vec::new(),
        surfaces: Vec::new(),
        matches: Vec::new(),
        version_matches: Vec::new(),
        audit: Default::default(),
    };

    cascade::promote_cascade_function_coverage_to_module_attributions(
        &rows,
        &BTreeMap::from([(ModuleId(10), fingerprints)]),
        &cascade_report,
        &mut report,
    );

    assert_eq!(report.matches.len(), 1);
    assert_eq!(
        report.matches[0].strategy,
        ModuleMatchStrategy::CascadeFunctionOwnership
    );
    assert!(
        !report.matches[0].external_importable,
        "weak structural-only function coverage proves ownership but must not wire an external import"
    );
    assert!(report.attributions.is_empty());
}

#[test]
fn exact_hint_import_proof_upgrades_existing_source_only_match_to_external() {
    let source = "function initPackage(){return helper(1);}";
    let mut rows = rows_with_package_source_at_version(source, "1.2.3");
    rows.modules[0].semantic_path = "modules/10-pkg.ts".to_string();
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg",
        "pkg@1.2.3/index.js",
        source,
    )];
    let mut report = VersionedPackageMatchReport {
        attributions: Vec::new(),
        surfaces: Vec::new(),
        matches: vec![PackageMatch {
            module_id: ModuleId(10),
            package_name: "pkg".to_string(),
            package_version: "1.2.3".to_string(),
            export_specifier: "pkg".to_string(),
            source_path: "cascade:pkg".to_string(),
            normalized_source_hash: String::new(),
            strategy: ModuleMatchStrategy::CascadeFunctionOwnership,
            function_signature_matches: 1,
            string_anchor_matches: 0,
            external_importable: false,
        }],
        version_matches: Vec::new(),
        audit: Default::default(),
    };

    exact_hint::promote_exact_hint_ownership_matches(&rows, &package_sources, &mut report);

    assert_eq!(report.matches.len(), 1);
    assert!(report.matches[0].external_importable);
    assert_eq!(report.attributions.len(), 1);
    assert_eq!(report.attributions[0].module_id, ModuleId(10));
    assert_eq!(
        report.attributions[0].export_specifier.as_deref(),
        Some("pkg")
    );
}

#[test]
fn exact_hint_import_proof_accepts_root_export_entry_path_match() {
    let source = "function saxParser(){return true;}";
    let mut rows = rows_with_package_source_at_version(source, "1.5.0");
    rows.modules[0].package_name = Some("sax".to_string());
    rows.modules[0].semantic_path = "modules/10-lib/sax.ts".to_string();
    let package_sources = [PackageSource::external(
        "sax",
        "1.5.0",
        "sax",
        "sax@1.5.0/lib/sax.js",
        source,
    )];
    let mut report = VersionedPackageMatchReport {
        attributions: Vec::new(),
        surfaces: Vec::new(),
        matches: Vec::new(),
        version_matches: Vec::new(),
        audit: Default::default(),
    };

    exact_hint::promote_exact_hint_ownership_matches(&rows, &package_sources, &mut report);

    assert_eq!(report.matches.len(), 1);
    assert!(report.matches[0].external_importable);
    assert_eq!(report.attributions.len(), 1);
    assert_eq!(report.attributions[0].package_name.as_str(), "sax");
    assert_eq!(
        report.attributions[0].export_specifier.as_deref(),
        Some("sax")
    );
}

#[test]
fn exact_hint_does_not_externalize_contradicted_root_hint_but_keeps_ownership() {
    // A module hinted `zod` at its package root would normally be externalized
    // directly here (short names leave no in-source token, so quality defaults
    // to `Trusted`). But its body is really `ws` code present in the index, so
    // the hint is provably mis-attributed. The direct externalization must be
    // suppressed, yet the source-only ownership match must remain so the
    // cross-package correction can later re-home the module to `ws`.
    let ws_source = "export function Receiver(){return \"permessage-deflate\";}";
    let mut rows = rows_with_package_source_at_version(ws_source, "3.22.5");
    rows.modules[0].package_name = Some("zod".to_string());
    rows.modules[0].semantic_path = "modules/10-zod/index.ts".to_string();
    let package_sources = [
        PackageSource::external(
            "zod",
            "3.22.5",
            "zod",
            "zod@3.22.5/index.js",
            "export const schema = { parse() { return 'schema'; } };",
        ),
        PackageSource::external(
            "ws",
            "8.16.0",
            "ws/receiver",
            "ws@8.16.0/lib/receiver.js",
            ws_source,
        ),
    ];
    let mut report = VersionedPackageMatchReport {
        attributions: Vec::new(),
        surfaces: Vec::new(),
        matches: Vec::new(),
        version_matches: Vec::new(),
        audit: Default::default(),
    };

    exact_hint::promote_exact_hint_ownership_matches(&rows, &package_sources, &mut report);

    let package_match = report
        .matches
        .iter()
        .find(|package_match| package_match.module_id == ModuleId(10))
        .expect("the source-only ownership match must be kept for cross-package correction");
    assert!(
        !package_match.external_importable,
        "a hint contradicted by another package's source must not be externalized from exact_hint"
    );
    assert!(
        report.attributions.is_empty(),
        "no external import attribution may be emitted for the contradicted hint"
    );
}

#[test]
fn pipeline_corrects_misattributed_hint_whose_semantic_path_resolves() {
    // A module wrapped under a bundler-misattributed `node_modules/zod/...`
    // path carries `package_name = zod`, and its semantic path *does* resolve
    // to a real `zod` import surface — so the hinted-package promotion fires
    // first and would emit a wrong `external_import zod`. But the module body
    // is really `ws` code present in the index. The proven cross-package
    // source match must override the wrong hint and re-home it to `ws`.
    let ws_source = "export function Receiver(options){const marker=\"permessage-deflate-extension-negotiation\";this.options=options;this.marker=marker;return this;}";
    let mut rows = rows_with_package_source_at_version(ws_source, "3.22.5");
    rows.modules[0].package_name = Some("zod".to_string());
    rows.modules[0].semantic_path = "modules/10-zod/helpers/parse.ts".to_string();
    let package_sources = [
        PackageSource::external(
            "zod",
            "3.22.5",
            "zod/internal/helpers/parse",
            "zod@3.22.5/dist/cjs/internal/helpers/parse.js",
            "export function parse(){return 'surface';}",
        ),
        PackageSource::external(
            "ws",
            "8.16.0",
            "ws/receiver",
            "ws@8.16.0/lib/receiver.js",
            ws_source,
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    let attribution = report
        .package_report
        .attributions
        .iter()
        .find(|attribution| attribution.module_id == ModuleId(10))
        .expect("module should be attributed to its proven package");
    assert_eq!(
        attribution.package_name.as_str(),
        "ws",
        "proven `ws` source must override the misattributed `zod` hint"
    );
    assert_eq!(attribution.package_version.as_deref(), Some("8.16.0"));
    assert!(
        !report
            .package_report
            .attributions
            .iter()
            .any(|attribution| attribution.module_id == ModuleId(10)
                && attribution.package_name.as_str() == "zod"),
        "the misattributed `zod` hint must not be emitted as an external import"
    );
}

#[test]
fn pipeline_promotes_structural_bag_with_unique_export_surface_to_external_import() {
    let mut rows = rows_with_package_source(
        r#"
        function firstAlpha(x){if(x){return true;}return false;}
        function firstBeta(y){if(y){return true;}return false;}
        "#,
    );
    rows.modules[0].semantic_path = "pkg/first.js".to_string();
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/first",
            "dist/first.js",
            "function one(value){if(value){return true;}return false;}",
        ),
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/second",
            "dist/second.js",
            "function two(input){if(input){return true;}return false;}",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    let package_match = report
        .package_report
        .matches
        .iter()
        .find(|package_match| {
            package_match.strategy == ModuleMatchStrategy::AggregateStructuralBagSimilarity
        })
        .expect("structural ownership should be present");
    assert!(
        !package_match.external_importable,
        "structural ownership plus a semantic surface is not enough to replace module source"
    );
    assert_eq!(package_match.export_specifier.as_str(), "pkg");
    assert!(report.package_report.attributions.is_empty());
}

#[test]
fn pipeline_externalizes_structural_non_root_hint_with_package_root() {
    let mut rows = rows_with_package_source(
        r#"
        function firstAlpha(x){if(x){return true;}return false;}
        function firstBeta(y){if(y){return true;}return false;}
        "#,
    );
    rows.modules[0].semantic_path = "pkg/first.js".to_string();
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg",
            "dist/index.js",
            "export const root = 1;",
        ),
        PackageSource::source_only(
            "pkg",
            "1.2.3",
            "pkg/internal-first",
            "dist/first.js",
            "function one(value){if(value){return true;}return false;}",
        ),
        PackageSource::source_only(
            "pkg",
            "1.2.3",
            "pkg/internal-second",
            "dist/second.js",
            "function two(input){if(input){return true;}return false;}",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    let package_match = report
        .package_report
        .matches
        .iter()
        .find(|package_match| {
            package_match.strategy == ModuleMatchStrategy::AggregateStructuralBagSimilarity
        })
        .expect("structural ownership should be present");
    assert!(
        !package_match.external_importable,
        "structural ownership must not fall back to the package root import"
    );
    assert!(report.package_report.attributions.is_empty());
}

#[test]
fn pipeline_externalizes_trusted_exact_hint_ownership() {
    let mut rows = rows_with_package_source_at_version("function sample(){return 42;}", "1.2.3");
    rows.modules[0].semantic_path = "pkg/sample.js".to_string();
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/other",
        "other.js",
        "export const other = 'unrelated-package-source';",
    )];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 1);
    assert_eq!(
        report.package_report.matches[0].strategy,
        ModuleMatchStrategy::DependencyClosureOwnership
    );
    assert!(
        report.package_report.matches[0]
            .source_path
            .contains("exact-hint:pkg@1.2.3:quality=trusted")
    );
    assert!(!report.package_report.matches[0].external_importable);
    assert!(report.package_report.attributions.is_empty());
}

#[test]
fn pipeline_externalizes_trusted_exact_hint_matching_public_subpath() {
    let mut rows = rows_with_package_source_at_version("function sample(){return 42;}", "1.2.3");
    rows.modules[0].semantic_path = "pkg/lib/sample.js".to_string();
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/lib/sample.js",
        "pkg@1.2.3/lib/sample.js",
        "export const sample = 'public-subpath-surface';",
    )];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 1);
    assert_eq!(
        report.package_report.matches[0].strategy,
        ModuleMatchStrategy::DependencyClosureOwnership
    );
    assert!(report.package_report.matches[0].external_importable);
    assert_eq!(
        report.package_report.matches[0].export_specifier.as_str(),
        "pkg/lib/sample.js"
    );
    assert_eq!(report.package_report.attributions.len(), 1);
}

#[test]
fn pipeline_promotes_dependency_hint_with_unique_subpath_surface_to_external_import() {
    let mut rows = rows_with_package_source_at_version("function sample(){return 42;}", "1.2.3");
    rows.modules[0].semantic_path = "pkg/sample.js".to_string();
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/sample",
        "dist/sample.js",
        "export const unrelated = 'public-subpath-surface';",
    )];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 1);
    assert_eq!(
        report.package_report.matches[0].strategy,
        ModuleMatchStrategy::DependencyClosureOwnership
    );
    assert!(report.package_report.matches[0].external_importable);
    assert_eq!(
        report.package_report.matches[0].export_specifier.as_str(),
        "pkg/sample"
    );
    assert_eq!(report.package_report.attributions.len(), 1);
}

#[test]
fn pipeline_disambiguates_semantic_build_variant_surfaces() {
    let mut rows =
        rows_with_package_source_at_version("function widget(){return 'widget';}", "1.2.3");
    rows.modules[0].semantic_path = "modules/10-pkg/export/widget.ts".to_string();
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/build/src/export/widget.js",
            "pkg@1.2.3/build/src/export/widget.js",
            "export const widget = 'src-variant';",
        ),
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/build/cjs/export/widget.js",
            "pkg@1.2.3/build/cjs/export/widget.js",
            "exports.widget = 'cjs-variant';",
        ),
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/build/esm/export/widget.js",
            "pkg@1.2.3/build/esm/export/widget.js",
            "export const widget = 'esm-variant';",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.attributions.len(), 1);
    let attribution = &report.package_report.attributions[0];
    assert_eq!(
        attribution.export_specifier.as_deref(),
        Some("pkg/build/esm/export/widget.js")
    );
    assert!(
        attribution
            .resolved_file
            .as_deref()
            .is_some_and(|resolved| resolved
                == "forced-external:semantic-source:build-variant:pkg@1.2.3/build/esm/export/widget.js"),
        "{attribution:?}"
    );
}

#[test]
fn pipeline_keeps_equal_rank_build_variants_source_only() {
    let mut rows =
        rows_with_package_source_at_version("function widget(){return 'widget';}", "1.2.3");
    rows.modules[0].semantic_path = "modules/10-pkg/export/widget.ts".to_string();
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/dist/esm/export/widget.js",
            "pkg@1.2.3/dist/esm/export/widget.js",
            "export const widget = 'dist-esm-variant';",
        ),
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/build/esm/export/widget.js",
            "pkg@1.2.3/build/esm/export/widget.js",
            "export const widget = 'build-esm-variant';",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert!(
        !report.package_report.matches[0].external_importable,
        "same-rank build variants remain ambiguous"
    );
    assert!(report.package_report.attributions.is_empty());
}

#[test]
fn pipeline_promotes_weak_structured_semantic_hint_to_unique_external_import() {
    let mut rows = rows_with_package_source_at_version("function q(a){return a;}", "7.8.2");
    rows.modules[0].package_name = Some("rxjs".to_string());
    rows.modules[0].semantic_path = "modules/10-rxjs/operators/sample.ts".to_string();
    let package_sources = [PackageSource::external(
        "rxjs",
        "7.8.2",
        "rxjs/internal/operators/sample",
        "rxjs@7.8.2/dist/cjs/internal/operators/sample.js",
        "export function sample(){return 'surface';}",
    )];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(
        report.package_report.matches[0].strategy,
        ModuleMatchStrategy::DependencyClosureOwnership
    );
    assert!(report.package_report.matches[0].external_importable);
    assert_eq!(
        report.package_report.matches[0].export_specifier.as_str(),
        "rxjs/internal/operators/sample"
    );
    assert_eq!(report.package_report.attributions.len(), 1);
}

#[test]
fn pipeline_keeps_weak_plain_semantic_hint_source_only() {
    let mut rows = rows_with_package_source_at_version("function q(a){return a;}", "1.2.3");
    rows.modules[0].semantic_path = "modules/10-sample.ts".to_string();
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/sample",
        "pkg@1.2.3/dist/sample.js",
        "export function sample(){return 'surface';}",
    )];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(
        report.package_report.matches[0].strategy,
        ModuleMatchStrategy::DependencyClosureOwnership
    );
    assert!(
        !report.package_report.matches[0].external_importable,
        "single-segment weak hints are not enough to wire an import"
    );
    assert!(report.package_report.attributions.is_empty());
}

#[test]
fn pipeline_externalizes_weak_module_with_exact_source_equivalence() {
    let source = "export function publicWidget(){return 'unique-source-equivalence';}";
    let mut rows = rows_with_package_source_at_version(source, "1.2.3");
    rows.modules[0].semantic_path = "modules/10-unrelated-hint.ts".to_string();
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/public-widget",
        "pkg@1.2.3/dist/public-widget.js",
        source,
    )];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 1);
    assert_eq!(
        report.package_report.matches[0].strategy,
        ModuleMatchStrategy::NormalizedSourceHash
    );
    assert!(report.package_report.matches[0].external_importable);
    assert_eq!(
        report.package_report.matches[0].export_specifier.as_str(),
        "pkg/public-widget"
    );
    assert_eq!(report.package_report.attributions.len(), 1);
}

#[test]
fn pipeline_promotes_weak_package_prefixed_leaf_hint_to_unique_external_import() {
    let mut rows = rows_with_package_source_at_version("function q(a){return a;}", "2.0.1");
    rows.modules[0].package_name = Some("color-convert".to_string());
    rows.modules[0].semantic_path = "modules/10-color-convert/conversions.ts".to_string();
    let package_sources = [PackageSource::external(
        "color-convert",
        "2.0.1",
        "color-convert/conversions.js",
        "color-convert@2.0.1/conversions.js",
        "export const conversions = {};",
    )];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(
        report.package_report.matches[0].strategy,
        ModuleMatchStrategy::DependencyClosureOwnership
    );
    assert!(report.package_report.matches[0].external_importable);
    assert_eq!(
        report.package_report.matches[0].export_specifier.as_str(),
        "color-convert/conversions.js"
    );
    assert_eq!(report.package_report.attributions.len(), 1);
}

#[test]
fn pipeline_promotes_build_segment_leaf_hint_to_unique_external_import() {
    let mut rows = rows_with_package_source_at_version("function FormData(){return 42;}", "4.0.5");
    rows.modules[0].package_name = Some("form-data".to_string());
    rows.modules[0].semantic_path = "modules/10-lib/form_data.ts".to_string();
    let package_sources = [PackageSource::external(
        "form-data",
        "4.0.5",
        "form-data",
        "form-data@4.0.5/lib/form_data.js",
        "export const unrelatedSurface = 1;",
    )];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(
        report.package_report.matches[0].strategy,
        ModuleMatchStrategy::DependencyClosureOwnership
    );
    assert!(report.package_report.matches[0].external_importable);
    assert_eq!(
        report.package_report.matches[0].export_specifier.as_str(),
        "form-data"
    );
    assert_eq!(report.package_report.attributions.len(), 1);
}

#[test]
fn pipeline_promotes_dependency_internal_kebab_hint_to_camel_internal_export() {
    let mut rows = rows_with_package_source_at_version("function arrayMap(){return 42;}", "1.2.3");
    rows.modules[0].semantic_path = "modules/10-pkg/_internal/array-map.ts".to_string();
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/_arrayMap.js",
        "_arrayMap.js",
        "export const unrelatedArrayMapSurface = 1;",
    )];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 1);
    assert_eq!(
        report.package_report.matches[0].strategy,
        ModuleMatchStrategy::DependencyClosureOwnership
    );
    assert!(report.package_report.matches[0].external_importable);
    assert_eq!(
        report.package_report.matches[0].export_specifier.as_str(),
        "pkg/_arrayMap.js"
    );
}

#[test]
fn pipeline_externalizes_token_only_internal_hint() {
    let mut rows = rows_with_package_source_at_version(
        "function unrelated(){return Array.isArray([]);}",
        "1.2.3",
    );
    rows.modules[0].semantic_path = "modules/10-pkg/_internal/is-typed-array.ts".to_string();
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/isTypedArray.js",
        "isTypedArray.js",
        "export const unrelatedIsTypedArraySurface = 1;",
    )];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 1);
    assert_eq!(
        report.package_report.matches[0].strategy,
        ModuleMatchStrategy::DependencyClosureOwnership
    );
    assert!(!report.package_report.matches[0].external_importable);
    assert!(report.package_report.attributions.is_empty());
}

#[test]
fn pipeline_externalizes_weak_internal_hint() {
    let mut rows = rows_with_package_source_at_version("function unrelated(){return 42;}", "1.2.3");
    rows.modules[0].semantic_path = "modules/10-pkg/_internal/array-map.ts".to_string();
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/_arrayMap.js",
        "_arrayMap.js",
        "export const unrelatedArrayMapSurface = 1;",
    )];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 1);
    assert_eq!(
        report.package_report.matches[0].strategy,
        ModuleMatchStrategy::DependencyClosureOwnership
    );
    assert!(!report.package_report.matches[0].external_importable);
    assert!(report.package_report.attributions.is_empty());
}

#[test]
fn pipeline_promotes_dependency_internal_filename_hint_to_export_subpath() {
    let mut rows = rows_with_package_source_at_version("function baseKeys(){return 42;}", "1.2.3");
    rows.modules[0].semantic_path = "modules/10-_baseKeys.ts".to_string();
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/_baseKeys.js",
        "_baseKeys.js",
        "export const unrelatedBaseKeysSurface = 1;",
    )];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 1);
    assert_eq!(
        report.package_report.matches[0].strategy,
        ModuleMatchStrategy::DependencyClosureOwnership
    );
    assert!(report.package_report.matches[0].external_importable);
    assert_eq!(
        report.package_report.matches[0].export_specifier.as_str(),
        "pkg/_baseKeys.js"
    );
}

#[test]
fn pipeline_externalizes_internal_filename_hint_without_source_anchor() {
    let mut rows = rows_with_package_source_at_version("function unrelated(){return 42;}", "1.2.3");
    rows.modules[0].semantic_path = "modules/10-_baseKeys.ts".to_string();
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/_baseKeys.js",
        "_baseKeys.js",
        "export const unrelatedBaseKeysSurface = 1;",
    )];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 1);
    assert_eq!(
        report.package_report.matches[0].strategy,
        ModuleMatchStrategy::DependencyClosureOwnership
    );
    assert!(!report.package_report.matches[0].external_importable);
    assert!(report.package_report.attributions.is_empty());
}

#[test]
fn pipeline_externalizes_plain_filename_hint_without_package_prefix() {
    let mut rows = rows_with_package_source_at_version("function map(){return 42;}", "1.2.3");
    rows.modules[0].semantic_path = "modules/10-map.ts".to_string();
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/map.js",
        "map.js",
        "export const unrelatedMapSurface = 1;",
    )];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 1);
    assert_eq!(
        report.package_report.matches[0].strategy,
        ModuleMatchStrategy::DependencyClosureOwnership
    );
    assert!(!report.package_report.matches[0].external_importable);
    assert!(report.package_report.attributions.is_empty());
}

#[test]
fn pipeline_forced_external_uses_plain_filename_semantic_surface() {
    let mut rows = rows_with_package_source_at_version("function basekeys(){return 42;}", "1.2.3");
    rows.modules[0].semantic_path = "modules/10-basekeys.ts".to_string();
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/basekeys.js",
        "basekeys.js",
        "export const unrelatedBasekeysSurface = 1;",
    )];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 1);
    assert!(
        !report.package_report.matches[0].external_importable,
        "plain filename similarity is only ownership evidence, not import replacement proof"
    );
    assert!(report.package_report.attributions.is_empty());
}

#[test]
fn pipeline_resolves_forced_external_target_by_export_surface() {
    let mut rows = rows_with_package_source_at_version("function publicApi(){return 42;}", "1.2.3");
    rows.modules[0].semantic_path = "pkg/public/api.js".to_string();
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/public/api",
            "pkg@1.2.3/dist/index.js",
            "export const unrelated = 'generic-build-entry';",
        ),
        PackageSource::source_only(
            "pkg",
            "1.2.3",
            "pkg/package.json",
            "pkg@1.2.3/package.json",
            r#"export default {"name":"pkg","exports":{"./public/api":"./dist/index.js"}};"#,
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 1);
    assert!(report.package_report.matches[0].external_importable);
    assert_eq!(
        report.package_report.matches[0].export_specifier.as_str(),
        "pkg/public/api"
    );
    assert_eq!(report.package_report.attributions.len(), 1);
    assert_eq!(report.package_report.surfaces.len(), 1);
    assert_eq!(
        report.package_report.surfaces[0].export_specifier.as_str(),
        "pkg/public/api"
    );
    assert!(
        report.package_report.surfaces[0]
            .evidence
            .as_deref()
            .is_some_and(|evidence| evidence.contains("cache-anchored-public-export"))
    );
}

#[test]
fn pipeline_keeps_source_only_ownership_without_verified_import_target() {
    let mut rows = rows_with_package_source_at_version("function unrelated(){return 42;}", "1.2.3");
    rows.modules[0].semantic_path = "pkg/sample.js".to_string();
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/other",
        "other.js",
        "export const other = 'unrelated-package-source';",
    )];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 1);
    assert!(!report.package_report.matches[0].external_importable);
    assert_eq!(
        report.package_report.matches[0].export_specifier.as_str(),
        "pkg"
    );
    assert!(
        report.package_report.matches[0]
            .source_path
            .starts_with("exact-hint:")
    );
    assert_eq!(report.package_report.attributions.len(), 0);
}

#[test]
fn pipeline_resolves_source_match_export_specifier_to_best_esm_package_source() {
    let matched_source = "export const sharedSurface = 1;";
    let mut rows = rows_with_package_source_at_version(matched_source, "1.2.3");
    rows.modules[0].semantic_path = "pkg/runtime.js".to_string();
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg",
            "pkg@1.2.3/build/src/index.js",
            matched_source,
        ),
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg",
            "pkg@1.2.3/build/esm/index.mjs",
            matched_source,
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 1);
    assert!(report.package_report.matches[0].external_importable);
    assert_eq!(
        report.package_report.matches[0].source_path.as_str(),
        "normalized-source-export:pkg@1.2.3/build/esm/index.mjs"
    );
    assert_eq!(
        report.package_report.attributions[0]
            .resolved_file
            .as_deref(),
        Some("normalized-source-export:pkg@1.2.3/build/esm/index.mjs")
    );
}

#[test]
fn resolver_maps_exact_hint_root_to_normalized_export_source() {
    let source = "export function fromPackage(){return 42;}";
    let module = ModuleInput::package(
        ModuleId(10),
        "pkgModule",
        "pkg/unknown.js",
        "pkg",
        Some("1.2.3".to_string()),
    );
    let package_match = PackageMatch {
        module_id: ModuleId(10),
        package_name: "pkg".to_string(),
        package_version: "1.2.3".to_string(),
        export_specifier: "pkg".to_string(),
        source_path: "exact-hint:pkg@1.2.3:quality=trusted".to_string(),
        normalized_source_hash: String::new(),
        strategy: ModuleMatchStrategy::DependencyClosureOwnership,
        function_signature_matches: 0,
        string_anchor_matches: 0,
        external_importable: false,
    };
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/submodule.js",
        "pkg@1.2.3/dist/esm/submodule.js",
        source,
    )];

    let target = resolve_external_import_target(
        &module,
        "pkg",
        "1.2.3",
        Some(&package_match),
        &package_sources,
        source,
    )
    .expect("normalized external source should resolve");

    assert_eq!(target.export_specifier.as_str(), "pkg/submodule.js");
    assert_eq!(
        target.source_path.as_str(),
        "normalized-source-export:pkg@1.2.3/dist/esm/submodule.js"
    );
}

#[test]
fn resolver_promotes_trusted_exact_hint_by_unique_source_fingerprint_match() {
    let module_source = r#"
        function publicOne(value) {
            if (value) return "stable-source-anchor-one";
            return "stable-source-anchor-two";
        }
        function publicTwo(input) {
            return input.map((item) => `${item}:stable-source-anchor-three`);
        }
    "#;
    let package_source = r#"
        function publicOne(value) {
            if (value) return "stable-source-anchor-one";
            return "stable-source-anchor-two";
        }
        function publicTwo(input) {
            return input.map((item) => `${item}:stable-source-anchor-three`);
        }
        exports.publicOne = publicOne;
        exports.publicTwo = publicTwo;
    "#;
    let module = ModuleInput::package(
        ModuleId(10),
        "pkgModule",
        "modules/10-minified.ts",
        "pkg",
        Some("1.2.3".to_string()),
    );
    let package_match = PackageMatch {
        module_id: ModuleId(10),
        package_name: "pkg".to_string(),
        package_version: "1.2.3".to_string(),
        export_specifier: "pkg".to_string(),
        source_path: "exact-hint:pkg@1.2.3:quality=trusted:semantic_path=modules/10-minified.ts"
            .to_string(),
        normalized_source_hash: String::new(),
        strategy: ModuleMatchStrategy::DependencyClosureOwnership,
        function_signature_matches: 0,
        string_anchor_matches: 0,
        external_importable: false,
    };
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/public",
        "pkg@1.2.3/dist/public.js",
        package_source,
    )];

    let target = resolve_external_import_target(
        &module,
        "pkg",
        "1.2.3",
        Some(&package_match),
        &package_sources,
        module_source,
    )
    .expect("trusted exact hint should resolve through unique source fingerprint evidence");

    assert_eq!(target.export_specifier.as_str(), "pkg/public");
    assert_eq!(
        target.source_path.as_str(),
        "forced-external:source-match:pkg@1.2.3/dist/public.js"
    );
}

#[test]
fn resolver_promotes_semantic_source_only_match_through_export_member_bridge() {
    let module = ModuleInput::package(
        ModuleId(10),
        "pkgWidget",
        "modules/10-pkg/internal/widget.ts",
        "pkg",
        Some("1.2.3".to_string()),
    );
    let package_match = PackageMatch {
        module_id: ModuleId(10),
        package_name: "pkg".to_string(),
        package_version: "1.2.3".to_string(),
        export_specifier: "pkg".to_string(),
        source_path:
            "exact-hint:pkg@1.2.3:quality=trusted:semantic_path=modules/10-pkg/internal/widget.ts"
                .to_string(),
        normalized_source_hash: String::new(),
        strategy: ModuleMatchStrategy::DependencyClosureOwnership,
        function_signature_matches: 0,
        string_anchor_matches: 0,
        external_importable: false,
    };
    let module_source = r#"
        function runtimeWidget(value) {
            return value ? "widget-runtime-anchor" : "widget-runtime-alternate";
        }
    "#;
    let package_sources = [
        PackageSource::source_only(
            "pkg",
            "1.2.3",
            "pkg/internal/widget",
            "pkg@1.2.3/dist/internal/widget.js",
            r#"
            function Widget(value) {
                return value ? "package-widget-anchor" : "package-widget-alternate";
            }
            function makeWidget(input) {
                return new Widget(input);
            }
            exports.Widget = Widget;
            exports.makeWidget = makeWidget;
            "#,
        ),
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg",
            "pkg@1.2.3/dist/index.js",
            r#"export { Widget, makeWidget } from "./internal/widget.js";"#,
        ),
    ];

    let target = resolve_external_import_target(
        &module,
        "pkg",
        "1.2.3",
        Some(&package_match),
        &package_sources,
        module_source,
    )
    .expect("semantic internal source should be wired through a proven public barrel");

    assert_eq!(target.export_specifier.as_str(), "pkg");
    assert!(
        target
            .source_path
            .contains("forced-external:export-members:barrel-reference:Widget,makeWidget:")
    );
    assert!(
        target.source_path.ends_with("pkg@1.2.3/dist/index.js"),
        "target should point at the importable public barrel"
    );
}

#[test]
fn resolver_promotes_source_only_match_when_semantic_hint_names_exported_member() {
    let module = ModuleInput::package(
        ModuleId(10),
        "opentelemetryDiagLogLevel",
        "modules/10-opentelemetry/api/diag-log-level.ts",
        "@opentelemetry/api",
        Some("1.9.1".to_string()),
    );
    let package_match = PackageMatch {
        module_id: ModuleId(10),
        package_name: "@opentelemetry/api".to_string(),
        package_version: "1.9.1".to_string(),
        export_specifier: "@opentelemetry/api".to_string(),
        source_path: "exact-hint:@opentelemetry/api@1.9.1:quality=trusted:semantic_path=modules/10-opentelemetry/api/diag-log-level.ts".to_string(),
        normalized_source_hash: String::new(),
        strategy: ModuleMatchStrategy::DependencyClosureOwnership,
        function_signature_matches: 0,
        string_anchor_matches: 0,
        external_importable: false,
    };
    let module_source = r#"
        Object.defineProperty(Dxq, "__esModule", { value: true });
        Dxq.DiagLogLevel = void 0;
        var DiagLogLevel;
        (function (DiagLogLevel) {
            DiagLogLevel[DiagLogLevel["NONE"] = 0] = "NONE";
            DiagLogLevel[DiagLogLevel["ERROR"] = 30] = "ERROR";
        })(DiagLogLevel = Dxq.DiagLogLevel || (Dxq.DiagLogLevel = {}));
    "#;
    let package_sources = [
        PackageSource::source_only(
            "@opentelemetry/api",
            "1.9.1",
            "@opentelemetry/api/build/src/diag/types",
            "build/src/diag/types.js",
            r#"
            Object.defineProperty(exports, "__esModule", { value: true });
            exports.DiagLogLevel = void 0;
            var DiagLogLevel;
            (function (DiagLogLevel) {
                DiagLogLevel[DiagLogLevel["NONE"] = 0] = "NONE";
                DiagLogLevel[DiagLogLevel["ERROR"] = 30] = "ERROR";
            })(DiagLogLevel = exports.DiagLogLevel || (exports.DiagLogLevel = {}));
            "#,
        ),
        PackageSource::source_only(
            "@opentelemetry/api",
            "1.9.1",
            "@opentelemetry/api/build/esm/index.js",
            "build/esm/index.js",
            r#"
            export { DiagLogLevel } from "./diag/types.js";
            export { trace } from "./trace-api.js";
            export { context } from "./context-api.js";
            export { propagation } from "./propagation-api.js";
            export { metrics } from "./metrics-api.js";
            export { diag } from "./diag-api.js";
            export { SpanKind } from "./trace/span_kind.js";
            export { SpanStatusCode } from "./trace/status.js";
            export { TraceFlags } from "./trace/trace_flags.js";
            "#,
        ),
        PackageSource::external(
            "@opentelemetry/api",
            "1.9.1",
            "@opentelemetry/api",
            "build/src/index.js",
            r#"
            Object.defineProperty(exports, "__esModule", { value: true });
            var types_1 = require("./diag/types");
            Object.defineProperty(exports, "DiagLogLevel", {
                enumerable: true,
                get: function () { return types_1.DiagLogLevel; }
            });
            "#,
        ),
    ];

    let target = resolve_external_import_target(
        &module,
        "@opentelemetry/api",
        "1.9.1",
        Some(&package_match),
        &package_sources,
        module_source,
    )
    .expect("trusted member-shaped semantic hint should bridge through public root export");

    assert_eq!(target.export_specifier.as_str(), "@opentelemetry/api");
    assert!(
        target
            .source_path
            .contains("forced-external:export-members:barrel-reference:DiagLogLevel:"),
        "{}",
        target.source_path
    );
    assert!(target.source_path.ends_with("build/src/index.js"));
}

#[test]
fn resolver_rejects_export_member_semantic_bridge_for_weak_hint() {
    let module = ModuleInput::package(
        ModuleId(10),
        "opentelemetryDiagLogLevel",
        "modules/10-opentelemetry/api/diag-log-level.ts",
        "@opentelemetry/api",
        Some("1.9.1".to_string()),
    );
    let package_match = PackageMatch {
        module_id: ModuleId(10),
        package_name: "@opentelemetry/api".to_string(),
        package_version: "1.9.1".to_string(),
        export_specifier: "@opentelemetry/api".to_string(),
        source_path: "exact-hint:@opentelemetry/api@1.9.1:quality=weak:semantic_path=modules/10-opentelemetry/api/diag-log-level.ts".to_string(),
        normalized_source_hash: String::new(),
        strategy: ModuleMatchStrategy::DependencyClosureOwnership,
        function_signature_matches: 0,
        string_anchor_matches: 0,
        external_importable: false,
    };
    let module_source = "exports.DiagLogLevel = void 0;";
    let package_sources = [
        PackageSource::source_only(
            "@opentelemetry/api",
            "1.9.1",
            "@opentelemetry/api/build/src/diag/types",
            "@opentelemetry/api@1.9.1/build/src/diag/types.js",
            "exports.DiagLogLevel = void 0;",
        ),
        PackageSource::external(
            "@opentelemetry/api",
            "1.9.1",
            "@opentelemetry/api",
            "@opentelemetry/api@1.9.1/build/src/index.js",
            r#"
            var types_1 = require("./diag/types");
            Object.defineProperty(exports, "DiagLogLevel", {
                enumerable: true,
                get: function () { return types_1.DiagLogLevel; }
            });
            "#,
        ),
    ];

    let target = resolve_external_import_target(
        &module,
        "@opentelemetry/api",
        "1.9.1",
        Some(&package_match),
        &package_sources,
        module_source,
    );

    assert_eq!(target, None);
}

#[test]
fn resolver_rejects_semantic_source_only_match_without_export_member_bridge() {
    let module = ModuleInput::package(
        ModuleId(10),
        "pkgWidget",
        "modules/10-pkg/internal/widget.ts",
        "pkg",
        Some("1.2.3".to_string()),
    );
    let package_match = PackageMatch {
        module_id: ModuleId(10),
        package_name: "pkg".to_string(),
        package_version: "1.2.3".to_string(),
        export_specifier: "pkg".to_string(),
        source_path:
            "exact-hint:pkg@1.2.3:quality=trusted:semantic_path=modules/10-pkg/internal/widget.ts"
                .to_string(),
        normalized_source_hash: String::new(),
        strategy: ModuleMatchStrategy::DependencyClosureOwnership,
        function_signature_matches: 0,
        string_anchor_matches: 0,
        external_importable: false,
    };
    let package_sources = [
        PackageSource::source_only(
            "pkg",
            "1.2.3",
            "pkg/internal/widget",
            "pkg@1.2.3/dist/internal/widget.js",
            "function Widget(){} exports.Widget = Widget;",
        ),
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg",
            "pkg@1.2.3/dist/index.js",
            "export const Widget = 1;",
        ),
    ];

    let target = resolve_external_import_target(
        &module,
        "pkg",
        "1.2.3",
        Some(&package_match),
        &package_sources,
        "const localWidget = 'widget-runtime-anchor';",
    );

    assert_eq!(target, None);
}

#[test]
fn resolver_rejects_root_export_without_source_equivalence() {
    let module = ModuleInput::package(
        ModuleId(10),
        "pkgRoot",
        "pkg",
        "pkg",
        Some("1.2.3".to_string()),
    );
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg",
        "pkg@1.2.3/index.js",
        "export const root = 1;",
    )];

    let target = resolve_external_import_target(
        &module,
        "pkg",
        "1.2.3",
        None,
        &package_sources,
        "function unrelated(){return 42;}",
    );

    assert_eq!(target, None);
}

#[test]
fn pipeline_does_not_externalize_without_package_sources() {
    let rows = rows_with_package_source("export function add(a,b){return a+b}");

    let report = match_packages_with_pipeline(&rows, &[], None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 0);
    assert_eq!(report.package_report.attributions.len(), 0);
}

#[test]
fn pipeline_externalizes_dependency_hint_when_export_surface_is_ambiguous() {
    let mut rows = rows_with_package_source_at_version("function sample(){return 42;}", "1.2.3");
    rows.modules[0].semantic_path = "pkg/sample.js".to_string();
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/sample",
            "dist/sample.js",
            "export const first = 1;",
        ),
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/sample",
            "esm/sample.js",
            "export const second = 2;",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 1);
    assert_eq!(
        report.package_report.matches[0].strategy,
        ModuleMatchStrategy::DependencyClosureOwnership
    );
    assert!(report.package_report.matches[0].external_importable);
    assert!(!report.package_report.attributions.is_empty());
}

#[test]
fn pipeline_externalizes_dependency_graph_source_fingerprint_match() {
    let module_source = r#"
        export function opaqueRuntime(value) {
            return ["opaque-alpha", "opaque-beta", "opaque-gamma", value].join(":");
        }
    "#;
    let mut rows = rows_with_package_source_at_version(module_source, "1.0.0");
    rows.modules[0].semantic_path = "modules/10-pkg/opaque.ts".to_string();
    rows.source_files.push(SourceFileInput::new(
        2,
        "dep-a.js",
        Some("export const depA = 'dep-a';".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        3,
        "dep-b.js",
        Some("export const depB = 'dep-b';".to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(11),
            "depA",
            "pkg/dep-a.js",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(12),
            "depB",
            "pkg/dep-b.js",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(3),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(10),
        target: ModuleDependencyTarget::Module(ModuleId(11)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(10),
        target: ModuleDependencyTarget::Module(ModuleId(12)),
    });
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(ModuleId(11), "pkg", "1.0.0", "pkg/dep-a")
            .with_resolved_file("pkg@1.0.0/lib/dep-a.js"),
    );
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(ModuleId(12), "pkg", "1.0.0", "pkg/dep-b")
            .with_resolved_file("pkg@1.0.0/lib/dep-b.js"),
    );
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/utility",
            "pkg@1.0.0/lib/utility.js",
            r#"
            const depA = require("./dep-a");
            const depB = require("./dep-b");
            exports.utility = function packageUtility(input) {
                return ["opaque-alpha", "opaque-beta", "opaque-gamma", input].join(":");
            };
            "#,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/other",
            "pkg@1.0.0/lib/other.js",
            r#"
            exports.other = function packageOther(input) {
                return ["opaque-alpha", "opaque-beta", "opaque-gamma", input].join(":");
            };
            "#,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/dep-a",
            "pkg@1.0.0/lib/dep-a.js",
            "exports.depA = 'dep-a';",
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/dep-b",
            "pkg@1.0.0/lib/dep-b.js",
            "exports.depB = 'dep-b';",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    let attribution = report
        .package_report
        .attributions
        .iter()
        .find(|attribution| attribution.module_id == ModuleId(10))
        .expect("dependency graph plus source strings should prove the source file");
    assert_eq!(attribution.export_specifier.as_deref(), Some("pkg/utility"));
    assert!(
        attribution
            .resolved_file
            .as_deref()
            .is_some_and(|resolved| resolved
                .starts_with("forced-external:dependency-graph-source:string-graph:")),
        "{attribution:?}"
    );
}

#[test]
fn pipeline_externalizes_dependency_neighborhood_source_match() {
    let mut rows = rows_with_package_source_at_version("var utility = tinyRuntime;", "1.0.0");
    rows.modules[0].semantic_path = "modules/10-pkg/tiny-runtime.ts".to_string();
    for (module_id, file_id, name, file_name) in [
        (ModuleId(11), 2, "depA", "dep-a.js"),
        (ModuleId(12), 3, "depB", "dep-b.js"),
    ] {
        rows.source_files.push(SourceFileInput::new(
            file_id,
            file_name,
            Some(format!("export const {name} = 1;")),
        ));
        rows.modules.push(
            ModuleInput::package(
                module_id,
                name,
                format!("pkg/{file_name}"),
                "pkg",
                Some("1.0.0".to_string()),
            )
            .with_source_file(file_id),
        );
    }
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(10),
        target: ModuleDependencyTarget::Module(ModuleId(11)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(10),
        target: ModuleDependencyTarget::Module(ModuleId(12)),
    });
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(ModuleId(11), "pkg", "1.0.0", "pkg/dep-a")
            .with_resolved_file("pkg@1.0.0/lib/dep-a.js"),
    );
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(ModuleId(12), "pkg", "1.0.0", "pkg/dep-b")
            .with_resolved_file("pkg@1.0.0/lib/dep-b.js"),
    );
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/utility",
            "pkg@1.0.0/lib/utility.js",
            "const depA = require('./dep-a'); const depB = require('./dep-b'); module.exports = 1;",
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/other",
            "pkg@1.0.0/lib/other.js",
            "const depA = require('./dep-a'); const extra = require('./extra'); module.exports = 1;",
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/dep-a",
            "pkg@1.0.0/lib/dep-a.js",
            "exports.depA = 1;",
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/dep-b",
            "pkg@1.0.0/lib/dep-b.js",
            "exports.depB = 1;",
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/extra",
            "pkg@1.0.0/lib/extra.js",
            "exports.extra = 1;",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    let attribution = report
        .package_report
        .attributions
        .iter()
        .find(|attribution| attribution.module_id == ModuleId(10))
        .expect("unique dependency neighborhood should prove tiny package source");
    assert_eq!(attribution.export_specifier.as_deref(), Some("pkg/utility"));
    assert!(
        attribution
            .resolved_file
            .as_deref()
            .is_some_and(|resolved| resolved
                .starts_with("forced-external:dependency-graph-source:dependency-neighborhood:")),
        "{attribution:?}"
    );
}

#[test]
fn pipeline_rejects_ambiguous_dependency_neighborhood_source_match() {
    let mut rows = rows_with_package_source_at_version("var utility = tinyRuntime;", "1.0.0");
    rows.modules[0].semantic_path = "modules/10-pkg/tiny-runtime.ts".to_string();
    for (module_id, file_id, name, file_name) in [
        (ModuleId(11), 2, "depA", "dep-a.js"),
        (ModuleId(12), 3, "depB", "dep-b.js"),
    ] {
        rows.source_files.push(SourceFileInput::new(
            file_id,
            file_name,
            Some(format!("export const {name} = 1;")),
        ));
        rows.modules.push(
            ModuleInput::package(
                module_id,
                name,
                format!("pkg/{file_name}"),
                "pkg",
                Some("1.0.0".to_string()),
            )
            .with_source_file(file_id),
        );
    }
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(10),
        target: ModuleDependencyTarget::Module(ModuleId(11)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(10),
        target: ModuleDependencyTarget::Module(ModuleId(12)),
    });
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(ModuleId(11), "pkg", "1.0.0", "pkg/dep-a")
            .with_resolved_file("pkg@1.0.0/lib/dep-a.js"),
    );
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(ModuleId(12), "pkg", "1.0.0", "pkg/dep-b")
            .with_resolved_file("pkg@1.0.0/lib/dep-b.js"),
    );
    let same_neighborhood =
        "const depA = require('./dep-a'); const depB = require('./dep-b'); module.exports = 1;";
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/first",
            "pkg@1.0.0/lib/first.js",
            same_neighborhood,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/second",
            "pkg@1.0.0/lib/second.js",
            same_neighborhood,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/dep-a",
            "pkg@1.0.0/lib/dep-a.js",
            "exports.depA = 1;",
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/dep-b",
            "pkg@1.0.0/lib/dep-b.js",
            "exports.depB = 1;",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert!(
        !report
            .package_report
            .attributions
            .iter()
            .any(|attribution| attribution.module_id == ModuleId(10)),
        "ambiguous dependency-neighborhood proof must not externalize"
    );
}

#[test]
fn pipeline_externalizes_dependency_graph_regex_fingerprint_match() {
    let module_source = r#"
        export function opaqueRuntime(value) {
            return [
                /^(?:[0-9a-f]{8})$/.test(value),
                /^(?:[0-9a-f]{4})$/.test(value),
                /^(?:[1-5][0-9a-f]{3})$/.test(value),
                /^(?:[89ab][0-9a-f]{3})$/.test(value),
                /^(?:[0-9a-f]{12})$/.test(value),
                /^(?:v?[0-9]+\\.[0-9]+\\.[0-9]+)$/.test(value),
                /^(?:alpha|beta|rc)\\.[0-9]+$/.test(value),
                /^(?:build|meta)\\.[0-9a-z-]+$/.test(value)
            ].some(Boolean);
        }
    "#;
    let mut rows = rows_with_package_source_at_version(module_source, "1.0.0");
    rows.modules[0].semantic_path = "modules/10-pkg/regex-runtime.ts".to_string();
    rows.source_files.push(SourceFileInput::new(
        2,
        "dep.js",
        Some("export const dep = 1;".to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(11),
            "dep",
            "pkg/dep.js",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(10),
        target: ModuleDependencyTarget::Module(ModuleId(11)),
    });
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(ModuleId(11), "pkg", "1.0.0", "pkg/dep")
            .with_resolved_file("pkg@1.0.0/lib/dep.js"),
    );
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/regex-runtime",
            "pkg@1.0.0/lib/regex-runtime.js",
            r#"
            const dep = require("./dep");
            exports.opaqueRuntime = function opaqueRuntime(value) {
                return [
                    /^(?:[0-9a-f]{8})$/.test(value),
                    /^(?:[0-9a-f]{4})$/.test(value),
                    /^(?:[1-5][0-9a-f]{3})$/.test(value),
                    /^(?:[89ab][0-9a-f]{3})$/.test(value),
                    /^(?:[0-9a-f]{12})$/.test(value),
                    /^(?:v?[0-9]+\\.[0-9]+\\.[0-9]+)$/.test(value),
                    /^(?:alpha|beta|rc)\\.[0-9]+$/.test(value),
                    /^(?:build|meta)\\.[0-9a-z-]+$/.test(value)
                ].some(Boolean);
            };
            "#,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/other",
            "pkg@1.0.0/lib/other.js",
            r#"
            const dep = require("./dep");
            exports.other = function other(value) {
                return [
                    /^(?:one|two|three)$/.test(value),
                    /^(?:four|five|six)$/.test(value)
                ].some(Boolean);
            };
            "#,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/dep",
            "pkg@1.0.0/lib/dep.js",
            "exports.dep = 1;",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    let attribution = report
        .package_report
        .attributions
        .iter()
        .find(|attribution| attribution.module_id == ModuleId(10))
        .expect("dependency graph plus regex anchors should prove the source file");
    assert_eq!(
        attribution.export_specifier.as_deref(),
        Some("pkg/regex-runtime")
    );
    assert!(
        attribution
            .resolved_file
            .as_deref()
            .is_some_and(|resolved| resolved
                .starts_with("forced-external:dependency-graph-source:string-graph:")),
        "{attribution:?}"
    );
}

#[test]
fn pipeline_iterates_dependency_graph_source_fingerprint_matches() {
    let module_a_source = r#"
        export function runtimeA(input) {
            return [
                "a-alpha", "a-beta", "a-gamma", "a-delta",
                "a-epsilon", "a-zeta", "a-eta", "a-theta",
                input
            ].join(":");
        }
    "#;
    let module_b_source = r#"
        export function runtimeB(input) {
            return [
                "b-alpha", "b-beta", "b-gamma", "b-delta",
                "b-epsilon", "b-zeta", "b-eta", "b-theta",
                input
            ].join(":");
        }
    "#;
    let mut rows = rows_with_package_source_at_version(module_a_source, "1.0.0");
    rows.modules[0].semantic_path = "modules/10-pkg/opaque-a.ts".to_string();
    rows.source_files.push(SourceFileInput::new(
        2,
        "b.js",
        Some(module_b_source.to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        3,
        "c.js",
        Some("export const seedC = 'seed-c';".to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(11),
            "b",
            "modules/11-pkg/opaque-b.ts",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(12),
            "c",
            "pkg/c.ts",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(3),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(10),
        target: ModuleDependencyTarget::Module(ModuleId(11)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(11),
        target: ModuleDependencyTarget::Module(ModuleId(12)),
    });
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(ModuleId(12), "pkg", "1.0.0", "pkg/c")
            .with_resolved_file("pkg@1.0.0/lib/c.js"),
    );
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/a",
            "pkg@1.0.0/lib/a.js",
            r#"
            const b = require("./b");
            exports.runtimeA = function runtimeA(input) {
                return [
                    "a-alpha", "a-beta", "a-gamma", "a-delta",
                    "a-epsilon", "a-zeta", "a-eta", "a-theta",
                    input
                ].join(":");
            };
            "#,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/b",
            "pkg@1.0.0/lib/b.js",
            r#"
            const c = require("./c");
            exports.runtimeB = function runtimeB(input) {
                return [
                    "b-alpha", "b-beta", "b-gamma", "b-delta",
                    "b-epsilon", "b-zeta", "b-eta", "b-theta",
                    input
                ].join(":");
            };
            "#,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/c",
            "pkg@1.0.0/lib/c.js",
            "exports.seedC = 'seed-c';",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    for (module_id, expected_specifier) in [(ModuleId(10), "pkg/a"), (ModuleId(11), "pkg/b")] {
        let attribution = report
            .package_report
            .attributions
            .iter()
            .find(|attribution| attribution.module_id == module_id)
            .unwrap_or_else(|| panic!("module {module_id:?} should be externalized"));
        assert_eq!(
            attribution.export_specifier.as_deref(),
            Some(expected_specifier)
        );
        assert!(
            attribution
                .resolved_file
                .as_deref()
                .is_some_and(|resolved| resolved
                    .starts_with("forced-external:dependency-graph-source:string-graph:")),
            "{attribution:?}"
        );
    }
}

#[test]
fn pipeline_rejects_ambiguous_dependency_graph_source_fingerprint_match() {
    let module_source = r#"
        export function opaqueRuntime(value) {
            return ["opaque-alpha", "opaque-beta", "opaque-gamma", value].join(":");
        }
    "#;
    let mut rows = rows_with_package_source_at_version(module_source, "1.0.0");
    rows.modules[0].semantic_path = "modules/10-pkg/opaque.ts".to_string();
    for (module_id, file_id, name, file_name) in [
        (ModuleId(11), 2, "depA", "dep-a.js"),
        (ModuleId(12), 3, "depB", "dep-b.js"),
    ] {
        rows.source_files.push(SourceFileInput::new(
            file_id,
            file_name,
            Some(format!("export const {name} = 1;")),
        ));
        rows.modules.push(
            ModuleInput::package(
                module_id,
                name,
                format!("pkg/{file_name}"),
                "pkg",
                Some("1.0.0".to_string()),
            )
            .with_source_file(file_id),
        );
    }
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(10),
        target: ModuleDependencyTarget::Module(ModuleId(11)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(10),
        target: ModuleDependencyTarget::Module(ModuleId(12)),
    });
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(ModuleId(11), "pkg", "1.0.0", "pkg/dep-a")
            .with_resolved_file("pkg@1.0.0/lib/dep-a.js"),
    );
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(ModuleId(12), "pkg", "1.0.0", "pkg/dep-b")
            .with_resolved_file("pkg@1.0.0/lib/dep-b.js"),
    );
    let ambiguous_source = r#"
        const depA = require("./dep-a");
        const depB = require("./dep-b");
        exports.value = function packageValue(input) {
            return ["opaque-alpha", "opaque-beta", "opaque-gamma", input].join(":");
        };
    "#;
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/first",
            "pkg@1.0.0/lib/first.js",
            ambiguous_source,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/second",
            "pkg@1.0.0/lib/second.js",
            ambiguous_source,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/dep-a",
            "pkg@1.0.0/lib/dep-a.js",
            "exports.depA = 'dep-a';",
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/dep-b",
            "pkg@1.0.0/lib/dep-b.js",
            "exports.depB = 'dep-b';",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert!(
        !report
            .package_report
            .attributions
            .iter()
            .any(|attribution| attribution.module_id == ModuleId(10)),
        "ambiguous graph/fingerprint proof must not externalize"
    );
}

#[test]
fn pipeline_externalizes_unique_dependency_edge_path_match() {
    let mut rows = rows_with_package_source_at_version("var tiny = 1;", "1.0.0");
    rows.modules[0].semantic_path = "modules/10-obfuscated.ts".to_string();
    rows.source_files.push(SourceFileInput::new(
        2,
        "entry.js",
        Some("export const entry = 1;".to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(11),
            "entry",
            "pkg/entry.js",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(11),
        target: ModuleDependencyTarget::Module(ModuleId(10)),
    });
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(ModuleId(11), "pkg", "1.0.0", "pkg/entry")
            .with_resolved_file("pkg@1.0.0/lib/entry.js"),
    );
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/entry",
            "pkg@1.0.0/lib/entry.js",
            "const tiny = require('./tiny'); exports.entry = tiny;",
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/tiny",
            "pkg@1.0.0/lib/tiny.js",
            "exports.tiny = 1;",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    let attribution = report
        .package_report
        .attributions
        .iter()
        .find(|attribution| attribution.module_id == ModuleId(10))
        .expect("unique package dependency edge path should externalize tiny module");
    assert_eq!(attribution.export_specifier.as_deref(), Some("pkg/tiny"));
    assert!(
        attribution
            .resolved_file
            .as_deref()
            .is_some_and(|resolved| resolved.starts_with("forced-external:dependency-edge-path:")),
        "{attribution:?}"
    );
}

#[test]
fn pipeline_rejects_ambiguous_dependency_edge_path_match() {
    let mut rows = rows_with_package_source_at_version("var tiny = 1;", "1.0.0");
    rows.modules[0].semantic_path = "modules/10-obfuscated.ts".to_string();
    rows.source_files.push(SourceFileInput::new(
        2,
        "entry.js",
        Some("export const entry = 1;".to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(11),
            "entry",
            "pkg/entry.js",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(11),
        target: ModuleDependencyTarget::Module(ModuleId(10)),
    });
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(ModuleId(11), "pkg", "1.0.0", "pkg/entry")
            .with_resolved_file("pkg@1.0.0/lib/entry.js"),
    );
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/entry",
            "pkg@1.0.0/lib/entry.js",
            "const tiny = require('./tiny'); const other = require('./other'); exports.entry = tiny;",
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/tiny",
            "pkg@1.0.0/lib/tiny.js",
            "exports.tiny = 1;",
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/other",
            "pkg@1.0.0/lib/other.js",
            "exports.other = 1;",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert!(
        !report
            .package_report
            .attributions
            .iter()
            .any(|attribution| attribution.module_id == ModuleId(10)),
        "multiple remaining package source dependency paths must stay source-only"
    );
}

#[test]
fn pipeline_corrects_wrong_package_hint_with_unique_exact_source() {
    let source = r#"
        export function realRuntime(input) {
            const marker = "real-package-stable-anchor";
            return `${marker}:${input}`;
        }
    "#;
    let mut rows = rows_with_package_source_at_version(source, "1.0.0");
    rows.modules[0].package_name = Some("wrong-pkg".to_string());
    rows.modules[0].semantic_path = "modules/10-wrong-pkg/opaque.ts".to_string();
    let package_sources = [
        PackageSource::external(
            "wrong-pkg",
            "1.0.0",
            "wrong-pkg/other",
            "wrong-pkg@1.0.0/lib/other.js",
            "export const unrelated = 'wrong-package-source';",
        ),
        PackageSource::external(
            "real-pkg",
            "2.0.0",
            "real-pkg/runtime",
            "real-pkg@2.0.0/lib/runtime.js",
            source,
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    let attribution = report
        .package_report
        .attributions
        .iter()
        .find(|attribution| attribution.module_id == ModuleId(10))
        .expect("unique exact source should override wrong package hint");
    assert_eq!(attribution.package_name.as_str(), "real-pkg");
    assert_eq!(attribution.package_version.as_deref(), Some("2.0.0"));
    assert_eq!(
        attribution.export_specifier.as_deref(),
        Some("real-pkg/runtime")
    );
    assert!(
        attribution
            .resolved_file
            .as_deref()
            .is_some_and(|resolved| resolved
                .starts_with("forced-external:cross-package-source:source-hash:")),
        "{attribution:?}"
    );
}

#[test]
fn pipeline_rejects_ambiguous_cross_package_exact_source_correction() {
    let source = r#"
        export function realRuntime(input) {
            const marker = "real-package-stable-anchor";
            return `${marker}:${input}`;
        }
    "#;
    let mut rows = rows_with_package_source_at_version(source, "1.0.0");
    rows.modules[0].package_name = Some("wrong-pkg".to_string());
    rows.modules[0].semantic_path = "modules/10-wrong-pkg/opaque.ts".to_string();
    let package_sources = [
        PackageSource::external(
            "wrong-pkg",
            "1.0.0",
            "wrong-pkg/other",
            "wrong-pkg@1.0.0/lib/other.js",
            "export const unrelated = 'wrong-package-source';",
        ),
        PackageSource::external(
            "real-pkg",
            "2.0.0",
            "real-pkg/runtime",
            "real-pkg@2.0.0/lib/runtime.js",
            source,
        ),
        PackageSource::external(
            "other-real-pkg",
            "2.0.0",
            "other-real-pkg/runtime",
            "other-real-pkg@2.0.0/lib/runtime.js",
            source,
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert!(
        !report
            .package_report
            .attributions
            .iter()
            .any(|attribution| attribution.module_id == ModuleId(10)),
        "duplicate exact source across packages must not correct ownership"
    );
}

#[test]
fn pipeline_corrects_exact_hint_version_with_cross_version_source_proof() {
    let source = r#"
        exports.actual = function actual() {
            return "runtime-token-anchor";
        };
    "#;
    let mut rows = rows_with_package_source_at_version(source, "1.0.0");
    rows.modules[0].semantic_path = "modules/10-pkg/runtime-token.ts".to_string();
    let package_sources = [
        PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg/legacy",
            "pkg@1.0.0/lib/legacy.js",
            "exports.legacy = 1;",
        ),
        PackageSource::external(
            "pkg",
            "2.0.0",
            "pkg/runtime",
            "pkg@2.0.0/lib/runtime.js",
            source,
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    let attribution = report
        .package_report
        .attributions
        .iter()
        .find(|attribution| attribution.module_id == ModuleId(10))
        .expect("cross-version source proof should correct the exact hint");
    assert_eq!(attribution.package_name.as_str(), "pkg");
    assert_eq!(attribution.package_version.as_deref(), Some("2.0.0"));
    assert_eq!(attribution.export_specifier.as_deref(), Some("pkg/runtime"));
    assert!(
        attribution
            .resolved_file
            .as_deref()
            .is_some_and(|resolved| resolved.contains("pkg@2.0.0/lib/runtime.js")),
        "{attribution:?}"
    );
}

#[test]
fn cross_version_source_proof_selects_unique_importable_version() {
    let source = r#"
        exports.actual = function actual() {
            return "runtime-token-anchor";
        };
    "#;
    let module = ModuleInput::package(
        ModuleId(10),
        "m10",
        "modules/10-pkg/runtime-token.ts",
        "pkg",
        Some("1.0.0".to_string()),
    );
    let package_match = PackageMatch {
        module_id: ModuleId(10),
        package_name: "pkg".to_string(),
        package_version: "1.0.0".to_string(),
        export_specifier: "pkg".to_string(),
        source_path:
            "exact-hint:pkg@1.0.0:quality=trusted:semantic_path=modules/10-pkg/runtime-token.ts"
                .to_string(),
        normalized_source_hash: String::new(),
        strategy: ModuleMatchStrategy::DependencyClosureOwnership,
        function_signature_matches: 0,
        string_anchor_matches: 0,
        external_importable: false,
    };
    let package_sources = [
        PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg/legacy",
            "pkg@1.0.0/lib/legacy.js",
            "exports.legacy = 1;",
        ),
        PackageSource::external(
            "pkg",
            "2.0.0",
            "pkg/runtime",
            "pkg@2.0.0/lib/runtime.js",
            source,
        ),
    ];
    let index = ExternalImportSourceIndex::build(&package_sources);
    let cache = ExternalImportProofScratch::default();

    let correction = same_package_cross_version_source_external_import_target(
        &module,
        &package_match,
        &index,
        source,
        &cache,
    )
    .expect("unique cross-version source proof should resolve");

    assert_eq!(correction.package_name.as_str(), "pkg");
    assert_eq!(correction.package_version.as_str(), "2.0.0");
    assert_eq!(correction.target.export_specifier.as_str(), "pkg/runtime");
    assert!(
        correction
            .target
            .source_path
            .starts_with("forced-external:cross-version-source:normalized_source_hash:"),
        "{}",
        correction.target.source_path
    );
}

#[test]
fn cross_version_source_proof_rejects_older_private_import_absent_from_hint_surface() {
    let source = r#"
        exports.privateValue = function privateValue() {
            return "runtime-token-anchor";
        };
    "#;
    let module = ModuleInput::package(
        ModuleId(10),
        "m10",
        "modules/10-pkg/private-token.ts",
        "pkg",
        Some("2.0.0".to_string()),
    );
    let package_match = PackageMatch {
        module_id: ModuleId(10),
        package_name: "pkg".to_string(),
        package_version: "2.0.0".to_string(),
        export_specifier: "pkg".to_string(),
        source_path:
            "exact-hint:pkg@2.0.0:quality=trusted:semantic_path=modules/10-pkg/private-token.ts"
                .to_string(),
        normalized_source_hash: String::new(),
        strategy: ModuleMatchStrategy::DependencyClosureOwnership,
        function_signature_matches: 0,
        string_anchor_matches: 0,
        external_importable: false,
    };
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/lib/private.js",
            "pkg@1.0.0/lib/private.js",
            source,
        ),
        PackageSource::source_only(
            "pkg",
            "2.0.0",
            "pkg/lib/private.js",
            "pkg@2.0.0/lib/private.js",
            source,
        ),
    ];
    let index = ExternalImportSourceIndex::build(&package_sources);
    let cache = ExternalImportProofScratch::default();

    let correction = same_package_cross_version_source_external_import_target(
        &module,
        &package_match,
        &index,
        source,
        &cache,
    );

    assert!(
        correction.is_none(),
        "older cross-version proof must not emit a private subpath absent from the hinted runtime export surface"
    );
}

#[test]
fn pipeline_rejects_ambiguous_cross_version_source_proof() {
    let source = r#"
        exports.actual = function actual() {
            return "runtime-token-anchor";
        };
    "#;
    let mut rows = rows_with_package_source_at_version(source, "1.0.0");
    rows.modules[0].semantic_path = "modules/10-pkg/runtime-token.ts".to_string();
    let package_sources = [
        PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg/legacy",
            "pkg@1.0.0/lib/legacy.js",
            "exports.legacy = 1;",
        ),
        PackageSource::external(
            "pkg",
            "2.0.0",
            "pkg/runtime",
            "pkg@2.0.0/lib/runtime.js",
            source,
        ),
        PackageSource::external(
            "pkg",
            "3.0.0",
            "pkg/runtime",
            "pkg@3.0.0/lib/runtime.js",
            source,
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert!(
        !report
            .package_report
            .attributions
            .iter()
            .any(|attribution| attribution.module_id == ModuleId(10)),
        "ambiguous cross-version source proof must not externalize"
    );
}

#[test]
fn pipeline_promotes_trusted_exact_hint_with_unique_root_surface_to_external_import() {
    let mut rows = rows_with_package_source_at_version("function sample(){return 42;}", "1.2.3");
    rows.modules[0].semantic_path = "pkg/index.js".to_string();
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg",
        "pkg@1.2.3/dist/index.js",
        "export const unrelated = 'public-root-surface';",
    )];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 1);
    assert_eq!(
        report.package_report.matches[0].strategy,
        ModuleMatchStrategy::DependencyClosureOwnership
    );
    assert!(
        report.package_report.matches[0]
            .source_path
            .contains("exact-hint:pkg@1.2.3:quality=trusted")
    );
    assert!(report.package_report.matches[0].external_importable);
    assert_eq!(
        report.package_report.matches[0].export_specifier.as_str(),
        "pkg"
    );
    assert_eq!(report.package_report.attributions.len(), 1);
    assert_eq!(
        report.package_report.attributions[0]
            .export_specifier
            .as_deref(),
        Some("pkg")
    );
}

#[test]
fn pipeline_promotes_exact_hint_with_public_export_member_proof() {
    let module_source = r#"
        function Widget() { return "widget-anchor"; }
        exports.Widget = Widget;
        "#;
    let module = ModuleInput::package(
        ModuleId(10),
        "widget",
        "pkg/private/widget-shim.js",
        "pkg",
        Some("1.2.3".to_string()),
    );
    let package_match = PackageMatch {
        module_id: ModuleId(10),
        package_name: "pkg".to_string(),
        package_version: "1.2.3".to_string(),
        export_specifier: "pkg".to_string(),
        source_path:
            "exact-hint:pkg@1.2.3:quality=trusted:semantic_path=pkg/private/widget-shim.js"
                .to_string(),
        normalized_source_hash: String::new(),
        strategy: ModuleMatchStrategy::DependencyClosureOwnership,
        function_signature_matches: 0,
        string_anchor_matches: 0,
        external_importable: false,
    };
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg",
        "pkg@1.2.3/dist/index.js",
        r#"
        const publicRoot = "public-root-surface";
        export function Widget() { return "widget-anchor"; }
        "#,
    )];

    let target = resolve_external_import_target(
        &module,
        "pkg",
        "1.2.3",
        Some(&package_match),
        &package_sources,
        module_source,
    )
    .expect("public export member proof should externalize exact hint");

    assert_eq!(target.export_specifier.as_str(), "pkg");
    assert!(
        target
            .source_path
            .starts_with("forced-external:public-export-members:"),
        "{}",
        target.source_path
    );
}

#[test]
fn exact_hint_generated_public_leaf_bridges_through_source_cache_export() {
    let module = ModuleInput::package(
        ModuleId(10),
        "trace_api",
        "trace-api",
        "@opentelemetry/api",
        Some("1.9.1".to_string()),
    );
    let package_match = PackageMatch {
        module_id: ModuleId(10),
        package_name: "@opentelemetry/api".to_string(),
        package_version: "1.9.1".to_string(),
        export_specifier: "@opentelemetry/api".to_string(),
        source_path:
            "exact-hint:@opentelemetry/api@1.9.1:quality=trusted:semantic_path=modules/10-trace-api.ts"
                .to_string(),
        normalized_source_hash: String::new(),
        strategy: ModuleMatchStrategy::DependencyClosureOwnership,
        function_signature_matches: 0,
        string_anchor_matches: 0,
        external_importable: false,
    };
    let module_source = r#"
        Object.defineProperty(exports, "__esModule", { value: true });
        exports.trace = void 0;
        exports.trace = TraceAPI.getInstance();
    "#;
    let package_sources = [
        PackageSource::source_only(
            "@opentelemetry/api",
            "1.9.1",
            "@opentelemetry/api/build/src/trace-api.js",
            "@opentelemetry/api@1.9.1/build/src/trace-api.js",
            r#"
            Object.defineProperty(exports, "__esModule", { value: true });
            exports.trace = void 0;
            exports.trace = TraceAPI.getInstance();
            "#,
        ),
        PackageSource::external(
            "@opentelemetry/api",
            "1.9.1",
            "@opentelemetry/api",
            "@opentelemetry/api@1.9.1/build/src/index.js",
            r#"
            const trace_api_1 = require("./trace-api");
            Object.defineProperty(exports, "trace", {
                enumerable: true,
                get: function () { return trace_api_1.trace; }
            });
            "#,
        ),
    ];

    let target = resolve_external_import_target(
        &module,
        "@opentelemetry/api",
        "1.9.1",
        Some(&package_match),
        &package_sources,
        module_source,
    )
    .expect("trusted generated public leaf should bridge through cached public export");

    assert_eq!(target.export_specifier.as_str(), "@opentelemetry/api");
    assert!(
        target
            .source_path
            .starts_with("forced-external:export-members:barrel-reference:trace:"),
        "{}",
        target.source_path
    );
}

#[test]
fn exact_hint_generated_leaf_bridge_requires_trusted_public_member_hint() {
    let module = ModuleInput::package(
        ModuleId(10),
        "qrcode",
        "qrcode",
        "qrcode",
        Some("1.5.3".to_string()),
    );
    let package_match = PackageMatch {
        module_id: ModuleId(10),
        package_name: "qrcode".to_string(),
        package_version: "1.5.3".to_string(),
        export_specifier: "qrcode".to_string(),
        source_path: "exact-hint:qrcode@1.5.3:quality=trusted:semantic_path=modules/10-qrcode.ts"
            .to_string(),
        normalized_source_hash: String::new(),
        strategy: ModuleMatchStrategy::DependencyClosureOwnership,
        function_signature_matches: 0,
        string_anchor_matches: 0,
        external_importable: false,
    };
    let module_source = "const bundledQrcodeWrapper = 1;";
    let package_sources = [
        PackageSource::source_only(
            "qrcode",
            "1.5.3",
            "qrcode/lib/index.js",
            "qrcode@1.5.3/lib/index.js",
            "exports.qrcode = function qrcode() {};",
        ),
        PackageSource::external(
            "qrcode",
            "1.5.3",
            "qrcode",
            "qrcode@1.5.3/index.js",
            "exports.qrcode = require('./lib/index.js').qrcode;",
        ),
    ];

    assert_eq!(
        resolve_external_import_target(
            &module,
            "qrcode",
            "1.5.3",
            Some(&package_match),
            &package_sources,
            module_source,
        ),
        None,
        "single-token generated filename hints are not public-import proof"
    );
}

#[test]
fn exact_hint_promotes_via_canonical_subpath_proof() {
    let module = ModuleInput::package(
        ModuleId(10),
        "gte",
        "modules/10-semver/functions/gte.ts",
        "semver",
        Some("7.6.3".to_string()),
    );
    let package_match = PackageMatch {
        module_id: ModuleId(10),
        package_name: "semver".to_string(),
        package_version: "7.6.3".to_string(),
        export_specifier: "semver".to_string(),
        source_path:
            "exact-hint:semver@7.6.3:quality=weak:semantic_path=modules/10-semver/functions/gte.ts"
                .to_string(),
        normalized_source_hash: String::new(),
        strategy: ModuleMatchStrategy::DependencyClosureOwnership,
        function_signature_matches: 0,
        string_anchor_matches: 0,
        external_importable: false,
    };
    let package_sources = [PackageSource::external(
        "semver",
        "7.6.3",
        "semver/functions/gte.js",
        "semver@7.6.3/functions/gte.js",
        "module.exports = function gte(a,b){ return true; };",
    )];

    let target = resolve_external_import_target(
        &module,
        "semver",
        "7.6.3",
        Some(&package_match),
        &package_sources,
        "",
    )
    .expect("canonical subpath proof should externalize exact hint");

    assert_eq!(target.export_specifier, "semver/functions/gte.js");
    assert!(
        target
            .source_path
            .starts_with("forced-external:canonical-subpath:"),
        "{}",
        target.source_path
    );
}

#[test]
fn pipeline_externalizes_weak_exact_hint_ownership() {
    let mut rows = rows_with_package_source_at_version("function unrelated(){return 42;}", "1.2.3");
    rows.modules[0].semantic_path = "pkg/sample.js".to_string();
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/other",
        "other.js",
        "export const other = 'unrelated-package-source';",
    )];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 1);
    assert_eq!(
        report.package_report.matches[0].strategy,
        ModuleMatchStrategy::DependencyClosureOwnership
    );
    assert!(
        report.package_report.matches[0]
            .source_path
            .contains("exact-hint:pkg@1.2.3:quality=weak")
    );
    assert!(!report.package_report.matches[0].external_importable);
    assert!(report.package_report.attributions.is_empty());
}

#[test]
fn pipeline_externalizes_weak_exact_hint_despite_other_package_neighbor() {
    let mut rows = rows_with_package_source_at_version("function unrelated(){return 42;}", "1.2.3");
    rows.modules[0].semantic_path = "pkg/sample.js".to_string();
    rows.source_files.push(SourceFileInput::new(
        2,
        "other.js",
        Some("export const otherDep = 1;".to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(11),
            "otherDep",
            "other/index.js",
            "other",
            Some("9.9.9".to_string()),
        )
        .with_source_file(2),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(10),
        target: ModuleDependencyTarget::Module(ModuleId(11)),
    });
    rows.package_attributions
        .push(PackageAttributionInput::accepted_external(
            ModuleId(11),
            "other",
            "9.9.9",
            "other",
        ));
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/other",
        "other.js",
        "export const other = 'unrelated-package-source';",
    )];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    let package_match = report
        .package_report
        .matches
        .iter()
        .find(|package_match| package_match.module_id == ModuleId(10))
        .expect("exact package hint should own the module even when imports point outside");
    assert!(
        package_match
            .source_path
            .contains("exact-hint:pkg@1.2.3:quality=weak"),
        "{}",
        package_match.source_path
    );
    assert!(!package_match.external_importable);
    assert!(
        !report
            .package_report
            .attributions
            .iter()
            .any(|attribution| {
                attribution.module_id == ModuleId(10)
                    && attribution.emission_mode
                        == reverts_input::PackageEmissionMode::ExternalImport
            }),
        "ownership-only evidence must not emit an unproven external import"
    );
}

#[test]
fn pipeline_promotes_exact_hint_without_fingerprinting_huge_package_source() {
    let mut rows = rows_with_package_source_at_version("function unrelated(){return 42;}", "1.2.3");
    rows.modules[0].semantic_path = "pkg/sample.js".to_string();
    let huge_source = format!(
        "export const oversized = \"{}\";",
        "x".repeat(PACKAGE_SOURCE_FINGERPRINT_MAX_BYTES + 1)
    );
    let package_sources = [PackageSource::source_only(
        "pkg",
        "1.2.3",
        "pkg/oversized",
        "oversized.js",
        huge_source,
    )];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 1);
    assert_eq!(
        report.package_report.matches[0].strategy,
        ModuleMatchStrategy::DependencyClosureOwnership
    );
    assert!(
        report.package_report.matches[0]
            .source_path
            .contains("exact-hint:pkg@1.2.3")
    );
    assert!(report.package_report.audit.is_clean());
}

#[test]
fn pipeline_externalizes_package_without_exact_version() {
    let mut rows = rows_with_package_source("function unrelated(){return 42;}");
    rows.modules[0].semantic_path = "pkg/sample.js".to_string();
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/other",
        "other.js",
        "export const other = 'unrelated-package-source';",
    )];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 0);
    assert_eq!(report.package_report.attributions.len(), 0);
}

#[test]
fn source_only_package_source_matches_without_external_attribution() {
    let rows = rows_with_package_source_at_version("export function add(a,b){return a+b}", "1.2.3");
    let package_sources = [PackageSource::source_only(
        "pkg",
        "1.2.3",
        "pkg/lib/add.js",
        "lib/add.js",
        "export function add(a, b) {\n  return a + b;\n}",
    )];

    let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

    assert!(report.audit.is_clean());
    assert!(
        report.attributions.is_empty(),
        "source-only package sources must not be externalized"
    );
    assert_eq!(report.matches.len(), 1);
    assert_eq!(report.matches[0].package_name, "pkg");
    assert_eq!(report.matches[0].package_version, "1.2.3");
    assert_eq!(report.matches[0].source_path, "lib/add.js");
    assert!(!report.matches[0].external_importable);
    match &report.version_matches[0] {
        BestVersionMatch::Selected { module_matches, .. } => {
            assert_eq!(module_matches.len(), 1);
            assert!(!module_matches[0].external_importable);
        }
        other => panic!("expected source-only match to select a version, got {other:?}"),
    }
}

#[test]
fn minified_prototype_shape_matches_unique_external_source() {
    let module_source = r#"
        var initMapCache = E(() => {
            depClear();
            depDelete();
            depGet();
            depHas();
            depSet();
            MinifiedCache.prototype.clear = clearImpl;
            MinifiedCache.prototype.delete = deleteImpl;
            MinifiedCache.prototype.get = getImpl;
            MinifiedCache.prototype.has = hasImpl;
            MinifiedCache.prototype.set = setImpl;
            exportedCache = MinifiedCache;
        });
    "#;
    let rows = rows_with_package_source_at_version(module_source, "1.0.0");
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/_MapCache.js",
            "pkg@1.0.0/_MapCache.js",
            r#"
                var mapClear = require('./_mapClear'),
                    mapDelete = require('./_mapDelete'),
                    mapGet = require('./_mapGet'),
                    mapHas = require('./_mapHas'),
                    mapSet = require('./_mapSet');
                function MapCache(values) { this.clear(); }
                MapCache.prototype.clear = mapClear;
                MapCache.prototype['delete'] = mapDelete;
                MapCache.prototype.get = mapGet;
                MapCache.prototype.has = mapHas;
                MapCache.prototype.set = mapSet;
                module.exports = MapCache;
            "#,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/_SetCache.js",
            "pkg@1.0.0/_SetCache.js",
            r#"
                function SetCache(values) { this.__data__ = []; }
                SetCache.prototype.add = cacheAdd;
                SetCache.prototype.push = cacheAdd;
                SetCache.prototype.has = cacheHas;
                module.exports = SetCache;
            "#,
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.attributions.len(), 1);
    let package_match = &report.package_report.matches[0];
    assert_eq!(
        package_match.strategy,
        ModuleMatchStrategy::PropertyShapeAndStringAnchors
    );
    assert!(package_match.external_importable);
    assert_eq!(package_match.export_specifier.as_str(), "pkg/_MapCache.js");
}

#[test]
fn minified_object_shape_matches_unique_external_source() {
    let module_source = r#"
        var parseResult = {
            raw: input,
            major: q,
            minor: K,
            patch: _,
            prerelease: z,
            build: Y,
            version: q + "." + K + "." + _
        };
        exports.parseResult = parseResult;
    "#;
    let rows = rows_with_package_source_at_version(module_source, "1.0.0");
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/parse",
            "pkg@1.0.0/lib/parse.js",
            r#"
                const parsed = {
                    raw: input,
                    major: major,
                    minor: minor,
                    patch: patch,
                    prerelease: prerelease,
                    build: build,
                    version: `${major}.${minor}.${patch}`
                };
                exports.parseResult = parsed;
            "#,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/format",
            "pkg@1.0.0/lib/format.js",
            r#"
                const formatted = {
                    source: input,
                    output: result,
                    options: options,
                    diagnostics: diagnostics
                };
                exports.formatted = formatted;
            "#,
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.attributions.len(), 1);
    let package_match = &report.package_report.matches[0];
    assert_eq!(
        package_match.strategy,
        ModuleMatchStrategy::ObjectShapeAndStringAnchors
    );
    assert!(package_match.external_importable);
    assert_eq!(package_match.export_specifier.as_str(), "pkg/parse");
}

#[test]
fn minified_object_shape_rejects_ambiguous_external_sources() {
    let module_source = r#"
        var parseResult = {
            raw: input,
            major: q,
            minor: K,
            patch: _,
            prerelease: z,
            build: Y,
            version: q + "." + K + "." + _
        };
        exports.parseResult = parseResult;
    "#;
    let rows = rows_with_package_source_at_version(module_source, "1.0.0");
    let source = r#"
        const parsed = {
            raw: input,
            major: major,
            minor: minor,
            patch: patch,
            prerelease: prerelease,
            build: build,
            version: `${major}.${minor}.${patch}`
        };
        exports.parseResult = parsed;
    "#;
    let package_sources = [
        PackageSource::external("pkg", "1.0.0", "pkg/parse-a", "pkg@1.0.0/lib/a.js", source),
        PackageSource::external("pkg", "1.0.0", "pkg/parse-b", "pkg@1.0.0/lib/b.js", source),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert!(
        !report
            .package_report
            .attributions
            .iter()
            .any(|attribution| attribution.module_id == ModuleId(10)),
        "ambiguous object-shape source proof must not externalize"
    );
}

#[test]
fn minified_class_shape_matches_unique_external_source() {
    let module_source = r#"
        class a {
            constructor(t) { this.u = t; }
            connect(t) { return this.open(t); }
            send(t) { return this.write(t); }
            close() { return this.shutdown(); }
            static from(t) { return new a(t); }
        }
        exports.Client = a;
    "#;
    let rows = rows_with_package_source_at_version(module_source, "1.0.0");
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/client",
            "pkg@1.0.0/dist/client.js",
            r#"
                export class Client {
                    constructor(endpoint) { this.endpoint = endpoint; }
                    connect(options) { return this.socket.connect(options); }
                    send(message) { return this.socket.send(message); }
                    close() { return this.socket.close(); }
                    static from(config) { return new Client(config.endpoint); }
                }
            "#,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/server",
            "pkg@1.0.0/dist/server.js",
            r#"
                export class Server {
                    constructor(endpoint) { this.endpoint = endpoint; }
                    listen(options) { return this.http.listen(options); }
                    stop() { return this.http.stop(); }
                    address() { return this.http.address(); }
                }
            "#,
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.attributions.len(), 1);
    let package_match = &report.package_report.matches[0];
    assert_eq!(
        package_match.strategy,
        ModuleMatchStrategy::ClassShapeAndStringAnchors
    );
    assert!(package_match.external_importable);
    assert_eq!(package_match.export_specifier.as_str(), "pkg/client");
}

#[test]
fn minified_class_shape_rejects_ambiguous_external_sources() {
    let module_source = r#"
        class a {
            connect(t) { return this.open(t); }
            send(t) { return this.write(t); }
            close() { return this.shutdown(); }
            static from(t) { return new a(t); }
        }
        exports.Client = a;
    "#;
    let rows = rows_with_package_source_at_version(module_source, "1.0.0");
    let source = r#"
        export class Client {
            connect(options) { return this.socket.connect(options); }
            send(message) { return this.socket.send(message); }
            close() { return this.socket.close(); }
            static from(config) { return new Client(config.endpoint); }
        }
    "#;
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/client-a",
            "pkg@1.0.0/dist/client-a.js",
            source,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/client-b",
            "pkg@1.0.0/dist/client-b.js",
            source,
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert!(
        !report
            .package_report
            .attributions
            .iter()
            .any(|attribution| attribution.module_id == ModuleId(10)),
        "ambiguous class-shape source proof must not externalize"
    );
}

#[test]
fn minified_switch_shape_matches_unique_external_source() {
    let module_source = r#"
        function c(t) {
            switch (t.kind) {
                case "major": return 1;
                case "minor": return 2;
                case "patch": return 3;
                case "prerelease": return 4;
                default: return 0;
            }
        }
        exports.classify = c;
    "#;
    let rows = rows_with_package_source_at_version(module_source, "1.0.0");
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/classify",
            "pkg@1.0.0/lib/classify.js",
            r#"
                export function classify(token) {
                    switch (token.type) {
                        case "major": return "M";
                        case "minor": return "m";
                        case "patch": return "p";
                        case "prerelease": return "pre";
                        default: return "";
                    }
                }
            "#,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/format",
            "pkg@1.0.0/lib/format.js",
            r#"
                export function format(token) {
                    switch (token.type) {
                        case "start": return "S";
                        case "stop": return "T";
                        case "pause": return "P";
                        case "resume": return "R";
                        default: return "";
                    }
                }
            "#,
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.attributions.len(), 1);
    let package_match = &report.package_report.matches[0];
    assert_eq!(
        package_match.strategy,
        ModuleMatchStrategy::SwitchShapeAndStringAnchors
    );
    assert!(package_match.external_importable);
    assert_eq!(package_match.export_specifier.as_str(), "pkg/classify");
}

#[test]
fn minified_switch_shape_rejects_ambiguous_external_sources() {
    let module_source = r#"
        function c(t) {
            switch (t.kind) {
                case "major": return 1;
                case "minor": return 2;
                case "patch": return 3;
                case "prerelease": return 4;
                default: return 0;
            }
        }
        exports.classify = c;
    "#;
    let rows = rows_with_package_source_at_version(module_source, "1.0.0");
    let source = r#"
        export function classify(token) {
            switch (token.type) {
                case "major": return "M";
                case "minor": return "m";
                case "patch": return "p";
                case "prerelease": return "pre";
                default: return "";
            }
        }
    "#;
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/classify-a",
            "pkg@1.0.0/lib/classify-a.js",
            source,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/classify-b",
            "pkg@1.0.0/lib/classify-b.js",
            source,
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert!(
        !report
            .package_report
            .attributions
            .iter()
            .any(|attribution| attribution.module_id == ModuleId(10)),
        "ambiguous switch-shape source proof must not externalize"
    );
}

#[test]
fn source_only_match_promotes_to_export_member_adapter_when_barrel_reexports_members() {
    let source = r#"
        function Widget() { return "widget-anchor"; }
        function makeWidget() { return new Widget(); }
        exports.Widget = Widget;
        exports.makeWidget = makeWidget;
    "#;
    let mut rows = rows_with_package_source_at_version(source, "1.0.0");
    rows.modules[0].semantic_path = "pkg/widget.js".to_string();
    let package_sources = [
        PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg/internal/widget",
            "pkg@1.0.0/dist-es/widget.js",
            source,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg",
            "pkg@1.0.0/dist-es/index.js",
            "export { Widget, makeWidget } from './widget.js';",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.attributions.len(), 1);
    let package_match = &report.package_report.matches[0];
    assert!(package_match.external_importable);
    assert_eq!(package_match.export_specifier.as_str(), "pkg");
    assert!(
        package_match
            .source_path
            .starts_with("forced-external:export-members:barrel-reference:"),
        "{}",
        package_match.source_path
    );
    assert_eq!(
        report.package_report.attributions[0]
            .resolved_file
            .as_deref(),
        Some(package_match.source_path.as_str())
    );
}

#[test]
fn package_source_public_export_proofs_include_source_only_reexported_members() {
    let source = r#"
        function Widget() { return "widget-anchor"; }
        function makeWidget() { return new Widget(); }
        exports.Widget = Widget;
        exports.makeWidget = makeWidget;
    "#;
    let package_sources = [
        PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg/internal/widget",
            "pkg@1.0.0/dist-es/widget.js",
            source,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg",
            "pkg@1.0.0/dist-es/index.js",
            "export { Widget, makeWidget } from './widget.js';",
        ),
    ];

    let proofs = package_source_public_export_proofs(&package_sources);

    assert_eq!(proofs.len(), 1);
    assert_eq!(proofs[0].export_specifier, "pkg");
    assert_eq!(proofs[0].source_path, "pkg@1.0.0/dist-es/widget.js");
    assert!(proofs[0].public_members.contains("Widget"));
    assert!(proofs[0].public_members.contains("makeWidget"));
}

#[test]
fn source_only_match_promotes_when_commonjs_root_reexports_matched_source() {
    let source = r#"
        function Widget() { return "widget-anchor"; }
        function makeWidget() { return new Widget(); }
        exports.Widget = Widget;
        exports.makeWidget = makeWidget;
    "#;
    let mut rows = rows_with_package_source_at_version(source, "1.0.0");
    rows.modules[0].semantic_path = "pkg/cjs/widget.development.js".to_string();
    let package_sources = [
        PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg/internal/widget",
            "pkg@1.0.0/cjs/widget.development.js",
            source,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg",
            "pkg@1.0.0/index.js",
            r#"
            'use strict';
            if (process.env.NODE_ENV === 'production') {
                module.exports = require('./cjs/widget.production.js');
            } else {
                module.exports = require('./cjs/widget.development.js');
            }
            "#,
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.attributions.len(), 1);
    let package_match = &report.package_report.matches[0];
    assert!(package_match.external_importable);
    assert_eq!(package_match.export_specifier.as_str(), "pkg");
    assert!(
        package_match
            .source_path
            .starts_with("forced-external:export-members:commonjs-reexport:"),
        "{}",
        package_match.source_path
    );
    assert!(
        package_match.source_path.contains("Widget")
            && package_match.source_path.contains("makeWidget"),
        "{}",
        package_match.source_path
    );
    assert_eq!(
        report.package_report.attributions[0]
            .resolved_file
            .as_deref(),
        Some(package_match.source_path.as_str())
    );
}

#[test]
fn source_only_match_promotes_when_export_star_reexports_matched_source() {
    let source = r#"
        function PublicWidget() { return "widget-anchor"; }
        function makePublicWidget() { return new PublicWidget(); }
        exports.PublicWidget = PublicWidget;
        exports.makePublicWidget = makePublicWidget;
    "#;
    let mut rows = rows_with_package_source_at_version(source, "1.0.0");
    rows.modules[0].semantic_path = "pkg/widget.js".to_string();
    let package_sources = [
        PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg/internal/widget",
            "pkg@1.0.0/dist/internal/widget.js",
            source,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg",
            "pkg@1.0.0/dist/index.js",
            "export * from './internal/widget.js';",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.attributions.len(), 1);
    let package_match = &report.package_report.matches[0];
    assert!(package_match.external_importable);
    assert_eq!(package_match.export_specifier.as_str(), "pkg");
    assert!(
        package_match
            .source_path
            .starts_with("forced-external:export-members:export-all-reexport:"),
        "{}",
        package_match.source_path
    );
    assert!(
        package_match.source_path.contains("PublicWidget")
            && package_match.source_path.contains("makePublicWidget"),
        "{}",
        package_match.source_path
    );
}

#[test]
fn source_only_match_promotes_when_export_star_chain_reexports_matched_source() {
    let source = r#"
        function PublicWidget() { return "widget-anchor"; }
        function makePublicWidget() { return new PublicWidget(); }
        exports.PublicWidget = PublicWidget;
        exports.makePublicWidget = makePublicWidget;
    "#;
    let mut rows = rows_with_package_source_at_version(source, "1.0.0");
    rows.modules[0].semantic_path = "pkg/widget.js".to_string();
    let package_sources = [
        PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg/internal/widget",
            "pkg@1.0.0/dist/internal/widget.js",
            source,
        ),
        PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg/public",
            "pkg@1.0.0/dist/public.js",
            "export * from './internal/widget.js';",
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg",
            "pkg@1.0.0/dist/index.js",
            "export * from './public.js';",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.attributions.len(), 1);
    let package_match = &report.package_report.matches[0];
    assert!(package_match.external_importable);
    assert_eq!(package_match.export_specifier.as_str(), "pkg");
    assert!(
        package_match
            .source_path
            .starts_with("forced-external:export-members:export-all-reexport:"),
        "{}",
        package_match.source_path
    );
    assert!(
        package_match.source_path.contains("PublicWidget")
            && package_match.source_path.contains("makePublicWidget"),
        "{}",
        package_match.source_path
    );
}

#[test]
fn source_only_match_promotes_when_named_reexport_chain_reaches_matched_source() {
    let source = r#"
        function PublicWidget() { return "widget-anchor"; }
        function makePublicWidget() { return new PublicWidget(); }
        exports.PublicWidget = PublicWidget;
        exports.makePublicWidget = makePublicWidget;
    "#;
    let mut rows = rows_with_package_source_at_version(source, "1.0.0");
    rows.modules[0].semantic_path = "pkg/widget.js".to_string();
    let package_sources = [
        PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg/internal/widget",
            "pkg@1.0.0/dist/internal/widget.js",
            source,
        ),
        PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg/public",
            "pkg@1.0.0/dist/public.js",
            "export { PublicWidget, makePublicWidget } from './internal/widget.js';",
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg",
            "pkg@1.0.0/dist/index.js",
            "export { PublicWidget, makePublicWidget } from './public.js';",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.attributions.len(), 1);
    let package_match = &report.package_report.matches[0];
    assert!(package_match.external_importable);
    assert_eq!(package_match.export_specifier.as_str(), "pkg");
    assert!(
        package_match
            .source_path
            .starts_with("forced-external:export-members:named-reexport:"),
        "{}",
        package_match.source_path
    );
    assert!(
        package_match.source_path.contains("PublicWidget")
            && package_match.source_path.contains("makePublicWidget"),
        "{}",
        package_match.source_path
    );
}

#[test]
fn source_only_match_promotes_when_commonjs_export_star_helper_reexports_matched_source() {
    let source = r#"
        function PublicWidget() { return "widget-anchor"; }
        exports.PublicWidget = PublicWidget;
    "#;
    let mut rows = rows_with_package_source_at_version(source, "1.0.0");
    rows.modules[0].semantic_path = "pkg/widget.js".to_string();
    let package_sources = [
        PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg/internal/widget",
            "pkg@1.0.0/dist-cjs/internal/widget.js",
            source,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg",
            "pkg@1.0.0/dist-cjs/index.js",
            r#"
            var __exportStar = function(m, exports) {
              for (var p in m) if (p !== "default") exports[p] = m[p];
            };
            __exportStar(require("./internal/widget.js"), exports);
            "#,
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.attributions.len(), 1);
    let package_match = &report.package_report.matches[0];
    assert!(package_match.external_importable);
    assert!(
        package_match
            .source_path
            .starts_with("forced-external:export-members:export-all-reexport:"),
        "{}",
        package_match.source_path
    );
}

#[test]
fn source_only_match_promotes_when_commonjs_member_require_reexports_matched_source() {
    let source = r#"
        function PublicWidget() { return "widget-anchor"; }
        exports.PublicWidget = PublicWidget;
    "#;
    let mut rows = rows_with_package_source_at_version(source, "1.0.0");
    rows.modules[0].semantic_path = "pkg/widget.js".to_string();
    let package_sources = [
        PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg/internal/widget",
            "pkg@1.0.0/dist-cjs/internal/widget.js",
            source,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg",
            "pkg@1.0.0/dist-cjs/index.js",
            r#"exports.PublicWidget = require("./internal/widget.js").PublicWidget;"#,
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.attributions.len(), 1);
    let package_match = &report.package_report.matches[0];
    assert!(package_match.external_importable);
    assert_eq!(package_match.export_specifier.as_str(), "pkg");
    assert!(
        package_match
            .source_path
            .starts_with("forced-external:export-members:barrel-reference:"),
        "{}",
        package_match.source_path
    );
}

#[test]
fn source_only_match_promotes_when_create_binding_reexports_matched_source() {
    let source = r#"
        function PublicWidget() { return "widget-anchor"; }
        exports.PublicWidget = PublicWidget;
    "#;
    let mut rows = rows_with_package_source_at_version(source, "1.0.0");
    rows.modules[0].semantic_path = "pkg/widget.js".to_string();
    let package_sources = [
        PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg/internal/widget",
            "pkg@1.0.0/dist-cjs/internal/widget.js",
            source,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg",
            "pkg@1.0.0/dist-cjs/index.js",
            r#"
            var __createBinding = function(o, m, k) {
              Object.defineProperty(o, k, { enumerable: true, get: function() { return m[k]; } });
            };
            __createBinding(exports, require("./internal/widget.js"), "PublicWidget");
            "#,
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.attributions.len(), 1);
    let package_match = &report.package_report.matches[0];
    assert!(package_match.external_importable);
    assert_eq!(package_match.export_specifier.as_str(), "pkg");
    assert!(
        package_match
            .source_path
            .starts_with("forced-external:export-members:barrel-reference:"),
        "{}",
        package_match.source_path
    );
}

#[test]
fn source_only_match_promotes_when_import_star_reexports_matched_source() {
    let source = r#"
        function PublicWidget() { return "widget-anchor"; }
        exports.PublicWidget = PublicWidget;
    "#;
    let mut rows = rows_with_package_source_at_version(source, "1.0.0");
    rows.modules[0].semantic_path = "pkg/widget.js".to_string();
    let package_sources = [
        PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg/internal/widget",
            "pkg@1.0.0/dist-cjs/internal/widget.js",
            source,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg",
            "pkg@1.0.0/dist-cjs/index.js",
            r#"
            var widget = __importStar(require("./internal/widget.js"));
            exports.PublicWidget = widget.PublicWidget;
            "#,
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.attributions.len(), 1);
    let package_match = &report.package_report.matches[0];
    assert!(package_match.external_importable);
    assert_eq!(package_match.export_specifier.as_str(), "pkg");
    assert!(
        package_match
            .source_path
            .starts_with("forced-external:export-members:barrel-reference:"),
        "{}",
        package_match.source_path
    );
}

#[test]
fn source_only_match_promotes_when_commonjs_reexport_chain_reaches_matched_source() {
    let source = r#"
        function PublicWidget() { return "widget-anchor"; }
        exports.PublicWidget = PublicWidget;
    "#;
    let mut rows = rows_with_package_source_at_version(source, "1.0.0");
    rows.modules[0].semantic_path = "pkg/widget.js".to_string();
    let package_sources = [
        PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg/internal/widget",
            "pkg@1.0.0/dist-cjs/internal/widget.js",
            source,
        ),
        PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg/public",
            "pkg@1.0.0/dist-cjs/public.js",
            r#"module.exports = require("./internal/widget.js");"#,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg",
            "pkg@1.0.0/dist-cjs/index.js",
            r#"module.exports = require("./public.js");"#,
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.attributions.len(), 1);
    let package_match = &report.package_report.matches[0];
    assert!(package_match.external_importable);
    assert_eq!(package_match.export_specifier.as_str(), "pkg");
    assert!(
        package_match
            .source_path
            .starts_with("forced-external:export-members:export-all-reexport:"),
        "{}",
        package_match.source_path
    );
}

#[test]
fn source_only_match_promotes_trusted_leaf_when_public_barrel_reexports_members() {
    let source = r#"
        class Alias {
            constructor(source) {
                this.source = source;
            }
        }
        exports.Alias = Alias;
    "#;
    let mut rows = rows_with_package_source_at_version(source, "2.7.0");
    rows.modules[0].semantic_path = "modules/10-yaml/alias.ts".to_string();
    rows.modules[0].package_name = Some("yaml".to_string());
    let package_sources = [
        PackageSource::source_only(
            "yaml",
            "2.7.0",
            "yaml/dist/nodes/Alias.js",
            "yaml@2.7.0/dist/nodes/Alias.js",
            source,
        ),
        PackageSource::external(
            "yaml",
            "2.7.0",
            "yaml",
            "yaml@2.7.0/dist/index.js",
            "var Alias = require('./nodes/Alias.js');\nexports.Alias = Alias.Alias;",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.attributions.len(), 1);
    let package_match = &report.package_report.matches[0];
    assert!(package_match.external_importable);
    assert_eq!(package_match.export_specifier.as_str(), "yaml");
    assert!(
        package_match
            .source_path
            .starts_with("forced-external:export-members:barrel-reference:Alias:"),
        "{}",
        package_match.source_path
    );
}

#[test]
fn source_only_leaf_ambiguity_is_resolved_by_unique_public_bridge() {
    let source = r#"
        function stringifyString(value) { return String(value); }
        exports.stringifyString = stringifyString;
    "#;
    let mut rows = rows_with_package_source_at_version(source, "2.7.0");
    rows.modules[0].semantic_path = "modules/10-yaml/stringify-string.ts".to_string();
    rows.modules[0].package_name = Some("yaml".to_string());
    let package_sources = [
        PackageSource::source_only(
            "yaml",
            "2.7.0",
            "yaml/browser/dist/stringify/stringifyString.js",
            "yaml@2.7.0/browser/dist/stringify/stringifyString.js",
            source,
        ),
        PackageSource::source_only(
            "yaml",
            "2.7.0",
            "yaml/dist/stringify/stringifyString.js",
            "yaml@2.7.0/dist/stringify/stringifyString.js",
            source,
        ),
        PackageSource::external(
            "yaml",
            "2.7.0",
            "yaml/util",
            "yaml@2.7.0/dist/util.js",
            "var stringifyString = require('./stringify/stringifyString.js');\nexports.stringifyString = stringifyString.stringifyString;",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.attributions.len(), 1);
    let package_match = &report.package_report.matches[0];
    assert!(package_match.external_importable);
    assert_eq!(package_match.export_specifier.as_str(), "yaml/util");
    assert!(
        package_match
            .source_path
            .starts_with("forced-external:export-members:barrel-reference:stringifyString:"),
        "{}",
        package_match.source_path
    );
}

#[test]
fn source_only_commonjs_module_exports_identifier_promotes_via_esm_wrapper() {
    let source = r#"
        class WebSocket {
            constructor(url) { this.url = url; }
            send(message) { return "ws:" + message; }
        }
        module.exports = WebSocket;
    "#;
    let mut rows = rows_with_package_source_at_version(source, "8.19.0");
    rows.modules[0].semantic_path = "modules/10-ws/websocket.ts".to_string();
    rows.modules[0].package_name = Some("ws".to_string());
    let package_sources = [
        PackageSource::source_only(
            "ws",
            "8.19.0",
            "ws/lib/websocket.js",
            "ws@8.19.0/lib/websocket.js",
            source,
        ),
        PackageSource::external(
            "ws",
            "8.19.0",
            "ws",
            "ws@8.19.0/wrapper.mjs",
            r#"
            import WebSocket from './lib/websocket.js';
            export { WebSocket };
            export default WebSocket;
            "#,
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.attributions.len(), 1);
    let package_match = &report.package_report.matches[0];
    assert!(package_match.external_importable);
    assert_eq!(package_match.export_specifier.as_str(), "ws");
    assert!(
        package_match
            .source_path
            .starts_with("forced-external:export-members:barrel-reference:WebSocket:"),
        "{}",
        package_match.source_path
    );
}

#[test]
fn export_member_adapter_proof_records_minified_member_aliases() {
    let module = ModuleInput::package(
        ModuleId(10),
        "init",
        "pkg/internal.js",
        "pkg",
        Some("1.0.0".to_string()),
    );
    let module_source = r#"
        var q, C;
        var init = E(() => {
            depInit();
            q = arrayToEnum(["alpha", "beta", "gamma"]);
            C = class C extends Error {
                constructor() {
                    super();
                    this.name = "PublicError";
                }
            };
        });
    "#;
    let public_source = r#"
        export const ErrorCode = arrayToEnum(["alpha", "beta", "gamma"]);
        export class PublicError extends Error {
            constructor() {
                super();
                this.name = "PublicError";
            }
        }
    "#;
    let package_match = PackageMatch {
        module_id: ModuleId(10),
        package_name: "pkg".to_string(),
        package_version: "1.0.0".to_string(),
        export_specifier: "pkg/internal".to_string(),
        source_path: "pkg@1.0.0/internal.js".to_string(),
        normalized_source_hash: String::new(),
        strategy: ModuleMatchStrategy::FunctionSignatureAndStringAnchors,
        function_signature_matches: 2,
        string_anchor_matches: 4,
        external_importable: false,
    };
    let package_sources = [
        PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg/internal",
            "pkg@1.0.0/internal.js",
            public_source,
        ),
        PackageSource::external("pkg", "1.0.0", "pkg", "pkg@1.0.0/index.js", public_source),
    ];

    let target = resolve_external_import_target(
        &module,
        "pkg",
        "1.0.0",
        Some(&package_match),
        &package_sources,
        module_source,
    )
    .expect("export-member alias proof should resolve to root import");

    assert_eq!(target.export_specifier.as_str(), "pkg");
    assert!(
        target
            .source_path
            .starts_with("forced-external:export-members:source-equivalent:"),
        "{}",
        target.source_path
    );
    assert!(
        target
            .source_path
            .contains(":aliases=C=PublicError,q=ErrorCode:"),
        "{}",
        target.source_path
    );
}

#[test]
fn export_member_adapter_rejects_barrel_without_source_reference() {
    let source = r#"
        function Widget() { return "widget-anchor"; }
        function makeWidget() { return new Widget(); }
        exports.Widget = Widget;
        exports.makeWidget = makeWidget;
    "#;
    let mut rows = rows_with_package_source_at_version(source, "1.0.0");
    rows.modules[0].semantic_path = "pkg/widget.js".to_string();
    let package_sources = [
        PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg/internal/widget",
            "pkg@1.0.0/dist-cjs/widget.js",
            source,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg",
            "pkg@1.0.0/dist-es/index.js",
            "export { Widget, makeWidget } from './different.js';",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 1);
    assert!(!report.package_report.matches[0].external_importable);
    assert!(report.package_report.attributions.is_empty());
}

#[test]
fn export_member_adapter_rejects_commonjs_reexport_to_different_source() {
    let source = r#"
        function Widget() { return "widget-anchor"; }
        function makeWidget() { return new Widget(); }
        exports.Widget = Widget;
        exports.makeWidget = makeWidget;
    "#;
    let mut rows = rows_with_package_source_at_version(source, "1.0.0");
    rows.modules[0].semantic_path = "pkg/cjs/widget.js".to_string();
    let package_sources = [
        PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg/internal/widget",
            "pkg@1.0.0/cjs/widget.js",
            source,
        ),
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg",
            "pkg@1.0.0/index.js",
            "module.exports = require('./cjs/different.js');",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 1);
    assert!(!report.package_report.matches[0].external_importable);
    assert!(report.package_report.attributions.is_empty());
}

#[test]
fn external_package_source_wins_over_duplicate_source_only_candidate() {
    let rows = rows_with_package_source_at_version("export function add(a,b){return a+b}", "1.2.3");
    let package_sources = [
        PackageSource::source_only(
            "pkg",
            "1.2.3",
            "pkg/add",
            "add.js",
            "export function add(a, b) {\n  return a + b;\n}",
        ),
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/add",
            "add.js",
            "export function add(a, b) {\n  return a + b;\n}",
        ),
    ];

    let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

    assert!(report.audit.is_clean());
    assert_eq!(report.attributions.len(), 1);
    assert_eq!(
        report.attributions[0].export_specifier.as_deref(),
        Some("pkg/add")
    );
    assert_eq!(report.matches.len(), 1);
    assert!(report.matches[0].external_importable);
}

#[test]
fn duplicate_exact_sources_prove_ownership_without_external_import() {
    let rows = rows_with_package_source_at_version("export function add(a,b){return a+b}", "1.2.3");
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/add",
            "add.js",
            "export function add(a, b) {\n  return a + b;\n}",
        ),
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/internal/add",
            "internal/add.js",
            "export function add(a, b) {\n  return a + b;\n}",
        ),
    ];

    let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

    assert!(report.audit.is_clean());
    assert!(
        report.attributions.is_empty(),
        "duplicate exact source bodies must not infer an import specifier"
    );
    assert_eq!(report.matches.len(), 1);
    assert_eq!(
        report.matches[0].strategy,
        ModuleMatchStrategy::NormalizedSourceHash
    );
    assert!(!report.matches[0].external_importable);
}

#[test]
fn pipeline_promotes_dependency_neighborhood_from_incoming_edges() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "wrapper.js",
        Some("var wrap = E(() => { return {}; });".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "one.js",
        Some("export function one(){return 'one-anchor';}".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        3,
        "two.js",
        Some("export function two(){return 'two-anchor';}".to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(10),
            "wrapper",
            "pkg/incoming-wrapper.js",
            "pkg",
            None,
        )
        .with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(ModuleId(11), "one", "pkg/one.js", "pkg", None).with_source_file(2),
    );
    rows.modules.push(
        ModuleInput::package(ModuleId(12), "two", "pkg/two.js", "pkg", None).with_source_file(3),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(11),
        target: ModuleDependencyTarget::Module(ModuleId(10)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(12),
        target: ModuleDependencyTarget::Module(ModuleId(10)),
    });
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/one",
            "one.js",
            "export function one(){return 'one-anchor';}",
        ),
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/two",
            "two.js",
            "export function two(){return 'two-anchor';}",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 3);
    let wrapper_match = report
        .package_report
        .matches
        .iter()
        .find(|package_match| package_match.module_id == ModuleId(10))
        .expect("incoming wrapper should be promoted");
    assert_eq!(
        wrapper_match.strategy,
        ModuleMatchStrategy::DependencyClosureOwnership
    );
    assert!(wrapper_match.source_path.contains("owned_neighbors=2/2"));
    assert!(wrapper_match.source_path.contains("out=0/0"));
    assert!(wrapper_match.source_path.contains("in=2/2"));
    assert!(!wrapper_match.external_importable);
}

#[test]
fn pipeline_iterates_dependency_neighborhood_ownership() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "first-wrapper.js",
        Some("var wrap = E(() => { one(); two(); });".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "one.js",
        Some("export function one(){return 'one-anchor';}".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        3,
        "two.js",
        Some("export function two(){return 'two-anchor';}".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        4,
        "second-wrapper.js",
        Some("var secondWrap = E(() => { wrap(); two(); });".to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(ModuleId(10), "wrapper", "pkg/first-wrapper.js", "pkg", None)
            .with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(ModuleId(11), "one", "pkg/one.js", "pkg", None).with_source_file(2),
    );
    rows.modules.push(
        ModuleInput::package(ModuleId(12), "two", "pkg/two.js", "pkg", None).with_source_file(3),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(13),
            "secondWrapper",
            "pkg/second-wrapper.js",
            "pkg",
            None,
        )
        .with_source_file(4),
    );
    for (from, to) in [
        (ModuleId(10), ModuleId(11)),
        (ModuleId(10), ModuleId(12)),
        (ModuleId(13), ModuleId(10)),
        (ModuleId(13), ModuleId(12)),
    ] {
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: from,
            target: ModuleDependencyTarget::Module(to),
        });
    }
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/one",
            "one.js",
            "export function one(){return 'one-anchor';}",
        ),
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/two",
            "two.js",
            "export function two(){return 'two-anchor';}",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 4);
    let second_wrapper_match = report
        .package_report
        .matches
        .iter()
        .find(|package_match| package_match.module_id == ModuleId(13))
        .expect("second wrapper should be promoted in a later round");
    assert!(
        second_wrapper_match
            .source_path
            .contains("owned_neighbors=2/2")
    );
    assert!(second_wrapper_match.source_path.contains("round=2"));
}

#[test]
fn pipeline_promotes_dependency_cluster_ownership() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    for (id, path, source) in [
        (
            1,
            "cluster-member.js",
            "var clusterMember = E(() => { one(); });",
        ),
        (2, "one.js", "export function one(){return 'one-anchor';}"),
        (3, "two.js", "export function two(){return 'two-anchor';}"),
        (
            4,
            "three.js",
            "export function three(){return 'three-anchor';}",
        ),
    ] {
        rows.source_files
            .push(SourceFileInput::new(id, path, Some(source.to_string())));
    }
    rows.modules.push(
        ModuleInput::package(
            ModuleId(10),
            "clusterMember",
            "pkg/cluster-member.js",
            "pkg",
            None,
        )
        .with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(ModuleId(11), "one", "pkg/one.js", "pkg", None).with_source_file(2),
    );
    rows.modules.push(
        ModuleInput::package(ModuleId(12), "two", "pkg/two.js", "pkg", None).with_source_file(3),
    );
    rows.modules.push(
        ModuleInput::package(ModuleId(13), "three", "pkg/three.js", "pkg", None)
            .with_source_file(4),
    );
    for (from, to) in [
        (ModuleId(10), ModuleId(11)),
        (ModuleId(11), ModuleId(12)),
        (ModuleId(12), ModuleId(13)),
    ] {
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: from,
            target: ModuleDependencyTarget::Module(to),
        });
    }
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/one",
            "one.js",
            "export function one(){return 'one-anchor';}",
        ),
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/two",
            "two.js",
            "export function two(){return 'two-anchor';}",
        ),
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/three",
            "three.js",
            "export function three(){return 'three-anchor';}",
        ),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(report.package_report.matches.len(), 4);
    let cluster_match = report
        .package_report
        .matches
        .iter()
        .find(|package_match| package_match.module_id == ModuleId(10))
        .expect("cluster member should be promoted");
    assert!(
        cluster_match
            .source_path
            .contains("dependency-cluster:pkg@1.2.3")
    );
    assert!(cluster_match.source_path.contains("owned_seeds=3/3"));
    assert!(!cluster_match.external_importable);
}

#[test]
fn pipeline_promotes_same_file_package_graph_ownership() {
    let one = "export function one(){return 'one-anchor';}";
    let gap = "const localValue = Math.random();";
    let two = "export function two(){return 'two-anchor';}";
    let tail = "const trailingValue = Date.now();";
    let bundled = [one, gap, two, tail].join("\n");
    let one_start = 0usize;
    let one_end = one.len();
    let gap_start = one_end + 1;
    let gap_end = gap_start + gap.len();
    let two_start = gap_end + 1;
    let two_end = two_start + two.len();
    let tail_start = two_end + 1;
    let tail_end = tail_start + tail.len();
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(bundled)));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(10),
            "one",
            "pkg/one.js",
            "pkg",
            Some("1.2.3".to_string()),
        )
        .with_source_file(1)
        .with_source_span(SourceSpan::new(one_start as u32, one_end as u32)),
    );
    rows.modules.push(
        ModuleInput::package(ModuleId(11), "gap", "pkg/absent-item.js", "pkg", None)
            .with_source_file(1)
            .with_source_span(SourceSpan::new(gap_start as u32, gap_end as u32)),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(12),
            "two",
            "pkg/two.js",
            "pkg",
            Some("1.2.3".to_string()),
        )
        .with_source_file(1)
        .with_source_span(SourceSpan::new(two_start as u32, two_end as u32)),
    );
    rows.modules.push(
        ModuleInput::package(ModuleId(13), "tail", "pkg/unused-tail.js", "pkg", None)
            .with_source_file(1)
            .with_source_span(SourceSpan::new(tail_start as u32, tail_end as u32)),
    );
    let package_sources = [
        PackageSource::external("pkg", "1.2.3", "pkg/one", "one.js", one),
        PackageSource::external("pkg", "1.2.3", "pkg/two", "two.js", two),
    ];

    let report = match_packages_with_pipeline(&rows, &package_sources, None);

    assert!(report.package_report.audit.is_clean());
    assert_eq!(
        report.package_report.matches.len(),
        4,
        "same-file package run should promote parseable modules without dependency edges"
    );
    let gap_match = report
        .package_report
        .matches
        .iter()
        .find(|package_match| package_match.module_id == ModuleId(11))
        .expect("same-file package graph should promote gap module");
    assert_eq!(
        gap_match.strategy,
        ModuleMatchStrategy::DependencyClosureOwnership
    );
    assert!(
        gap_match
            .source_path
            .contains("package-file-graph:pkg@1.2.3"),
        "{}",
        gap_match.source_path
    );
    assert!(gap_match.source_path.contains("owned_seeds=2/2"));
    assert!(gap_match.source_path.contains("run_size=4"));
    assert!(!gap_match.external_importable);
}

#[test]
fn unversioned_exact_match_does_not_infer_package_version() {
    let rows = rows_with_package_source("export function add(a,b){return a+b}");
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.2.3",
            "pkg/add",
            "add.js",
            "export function add(a, b) { return a + b; }",
        ),
        PackageSource::external(
            "pkg",
            "2.0.0",
            "pkg/add",
            "add.js",
            "export function add(a, b) { return a + b; }",
        ),
    ];

    let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

    assert!(report.attributions.is_empty());
    assert!(report.matches.is_empty());
    assert!(report.version_matches.is_empty());
    assert!(report.audit.is_clean());
}

#[test]
fn matcher_and_generation_share_source_slice_semantics() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "bundle.js",
        Some("export const one = 1;\nexport const two = 2;".to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(10),
            "one",
            "pkg/one.ts",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1)
        .with_source_span(SourceSpan::new(0, 21)),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(11),
            "two",
            "pkg/two.ts",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1)
        .with_source_span(SourceSpan::new(22, 43)),
    );
    let package_sources = [PackageSource::external(
        "pkg",
        "1.0.0",
        "pkg/two",
        "two.js",
        "export const two = 2;",
    )];

    let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

    assert_eq!(report.attributions.len(), 1);
    assert_eq!(report.attributions[0].module_id, ModuleId(11));
}

#[test]
fn accepted_package_attribution_is_not_recomputed_in_parallel() {
    let mut rows = rows_with_package_source("export function add(a,b){return a+b}");
    rows.package_attributions
        .push(PackageAttributionInput::accepted_external(
            ModuleId(10),
            "pkg",
            "1.2.3",
            "pkg/add",
        ));
    let package_sources = [PackageSource::external(
        "pkg",
        "1.2.3",
        "pkg/add",
        "add.js",
        "export function add(a, b) { return a + b; }",
    )];

    let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

    assert!(report.attributions.is_empty());
    assert!(report.matches.is_empty());
    assert!(report.audit.is_clean());
}

#[test]
fn versioned_matcher_uses_exact_package_version_hint_over_sorted_hashes() {
    let package_sources = [
        PackageSource::external("pkg", "1.0.0", "pkg/a", "a.js", "export const a = 1;"),
        PackageSource::external(
            "pkg",
            "2.0.0",
            "pkg/target",
            "target.js",
            "export const target = 42;",
        ),
        PackageSource::external("pkg", "3.0.0", "pkg/z", "z.js", "export const z = 26;"),
    ];
    let rows = rows_with_package_source_at_version("export const target=42", "2.0.0");
    let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

    assert!(report.audit.is_clean());
    assert_eq!(report.attributions.len(), 1);
    assert_eq!(
        report.attributions[0].package_version.as_deref(),
        Some("2.0.0")
    );
    assert_eq!(
        report.matches[0].strategy,
        ModuleMatchStrategy::NormalizedSourceHash
    );
}

#[test]
fn versioned_matcher_uses_explicit_package_version_for_module_group() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "bundle.js",
        Some("export const one = 1;\nexport const two = 2;".to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(10),
            "one",
            "pkg/one.ts",
            "pkg",
            Some("2.0.0".to_string()),
        )
        .with_source_file(1)
        .with_source_span(SourceSpan::new(0, 21)),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(11),
            "two",
            "pkg/two.ts",
            "pkg",
            Some("2.0.0".to_string()),
        )
        .with_source_file(1)
        .with_source_span(SourceSpan::new(22, 43)),
    );
    let package_sources = [
        PackageSource::external("pkg", "1.0.0", "pkg/one", "one.js", "export const one = 1;"),
        PackageSource::external("pkg", "2.0.0", "pkg/one", "one.js", "export const one = 1;"),
        PackageSource::external("pkg", "2.0.0", "pkg/two", "two.js", "export const two = 2;"),
    ];

    let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

    assert!(report.audit.is_clean());
    assert_eq!(report.attributions.len(), 2);
    assert!(
        report
            .attributions
            .iter()
            .all(|attribution| attribution.package_version.as_deref() == Some("2.0.0"))
    );
    let selected = report
        .version_matches
        .iter()
        .find_map(|decision| match decision {
            BestVersionMatch::Selected { score, .. } => Some(score),
            _ => None,
        })
        .expect("exact version should be selected");
    assert_eq!(selected.package_version, "2.0.0");
    assert_eq!(selected.total_modules, 2);
    assert_eq!(selected.matched_modules, 2);
    assert!(selected.binary_search_probes > 0);
}

#[test]
fn versioned_matcher_uses_exact_module_version_hints_per_version() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "bundle-one.js",
        Some("export const one=1;".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "bundle-two.js",
        Some("export const two=2;".to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(10),
            "one",
            "node_modules/pkg/one.js",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(11),
            "two",
            "node_modules/pkg/two.js",
            "pkg",
            Some("2.0.0".to_string()),
        )
        .with_source_file(2),
    );
    let package_sources = [
        PackageSource::external("pkg", "1.0.0", "pkg/one", "one.js", "export const one = 1;"),
        PackageSource::external("pkg", "2.0.0", "pkg/two", "two.js", "export const two = 2;"),
    ];

    let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

    assert!(report.audit.is_clean());
    assert_eq!(report.attributions.len(), 2);
    assert_eq!(
        report
            .attributions
            .iter()
            .map(|attribution| (
                attribution.module_id,
                attribution.package_version.as_deref()
            ))
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([(ModuleId(10), Some("1.0.0")), (ModuleId(11), Some("2.0.0")),])
    );
    let selected_versions = report
        .version_matches
        .iter()
        .filter_map(|decision| match decision {
            BestVersionMatch::Selected { score, .. } => Some(score.package_version.as_str()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(selected_versions, BTreeSet::from(["1.0.0", "2.0.0"]));
}

#[test]
fn unversioned_equal_sources_do_not_infer_package_version() {
    let rows = rows_with_package_source("export const value=1");
    let package_sources = [
        PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/value",
            "value.js",
            "export const value = 1;",
        ),
        PackageSource::external(
            "pkg",
            "2.0.0",
            "pkg/value",
            "value.js",
            "export const value = 1;",
        ),
    ];

    let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

    assert!(report.attributions.is_empty());
    assert!(report.matches.is_empty());
    assert!(report.version_matches.is_empty());
    assert!(report.audit.is_clean());
}

#[test]
fn versioned_matcher_can_match_by_function_signatures_and_string_anchors() {
    let rows = rows_with_package_source_at_version(
        "const bundleMarker = 1;\nexport function first(){return 'stable-anchor'}\nexport function second(){return 'other-anchor'}",
        "1.0.0",
    );
    let package_sources = [PackageSource::external(
        "pkg",
        "1.0.0",
        "pkg/functions",
        "functions.js",
        "function first(){return 'stable-anchor'}\nfunction second(){return 'other-anchor'}",
    )];

    let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

    assert!(report.audit.is_clean());
    assert_eq!(report.attributions.len(), 1);
    assert_eq!(
        report.matches[0].strategy,
        ModuleMatchStrategy::FunctionSignatureAndStringAnchors
    );
    assert!(report.matches[0].function_signature_matches >= 2);
    assert!(report.matches[0].string_anchor_matches >= 1);
}

#[test]
fn versioned_matcher_can_match_by_regex_anchors() {
    let rows = rows_with_package_source_at_version(
        "export function first(v){return /^(?:[0-9a-f]{8})$/.test(v)}\nexport function second(v){return /^(?:alpha|beta|rc)\\.[0-9]+$/.test(v)}",
        "1.0.0",
    );
    let package_sources = [PackageSource::external(
        "pkg",
        "1.0.0",
        "pkg/regex",
        "regex.js",
        "const packageOnly = true;\nfunction first(v){return /^(?:[0-9a-f]{8})$/.test(v)}\nfunction second(v){return /^(?:alpha|beta|rc)\\.[0-9]+$/.test(v)}",
    )];

    let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

    assert!(report.audit.is_clean());
    assert_eq!(report.attributions.len(), 1);
    assert_eq!(
        report.matches[0].strategy,
        ModuleMatchStrategy::FunctionSignatureAndStringAnchors
    );
    assert!(report.matches[0].function_signature_matches >= 2);
    assert!(report.matches[0].string_anchor_matches >= 2);
}

#[test]
fn versioned_matcher_can_match_by_export_member_anchors() {
    let rows = rows_with_package_source_at_version(
        "function alpha(q){return q + 1;}\nfunction beta(q){return q - 1;}\nexports.parseVersion = alpha;\nexports.formatVersion = beta;",
        "1.0.0",
    );
    let package_sources = [PackageSource::external(
        "pkg",
        "1.0.0",
        "pkg/version-tools",
        "version-tools.js",
        "const packageOnly = true;\nfunction alpha(q){return q + 1;}\nfunction beta(q){return q - 1;}\nmodule.exports = { parseVersion: alpha, formatVersion: beta };",
    )];

    let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

    assert!(report.audit.is_clean());
    assert_eq!(report.attributions.len(), 1);
    assert_eq!(
        report.matches[0].strategy,
        ModuleMatchStrategy::FunctionSignatureAndStringAnchors
    );
    assert!(report.matches[0].function_signature_matches >= 2);
    assert!(report.matches[0].string_anchor_matches >= 2);
}

#[test]
fn source_package_imports_are_extracted_from_whole_source_file() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "bundle.js",
        Some(
            "import { x } from 'pkg/sub';\nconst y = require('undici');\nasync function f(){ return import('ws'); }\nimport fs from 'node:fs';"
                .to_string(),
        ),
    ));

    let names = package_import_names_from_sources(&rows)
        .expect("source-backed package imports should parse");

    assert!(names.contains("pkg"));
    assert!(names.contains("undici"));
    assert!(names.contains("ws"));
    assert!(!names.contains("node:fs"));
    assert!(!names.contains("fs"));
}

#[test]
fn source_backed_import_surface_uses_unique_project_package_version() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "bundle.js",
        Some("const client = require('undici');".to_string()),
    ));
    rows.modules.push(ModuleInput::package(
        ModuleId(10),
        "undici_wrapper",
        "pkg/undici.ts",
        "undici",
        Some("2.2.1".to_string()),
    ));
    rows.package_attributions
        .push(PackageAttributionInput::accepted_external(
            ModuleId(10),
            "undici",
            "2.2.1",
            "undici",
        ));

    let report = VersionedPackageMatcher::default().match_rows(&rows, &[]);

    assert!(report.audit.is_clean());
    assert_eq!(report.surfaces.len(), 1);
    assert_eq!(report.surfaces[0].package_name, "undici");
    assert_eq!(report.surfaces[0].package_version.as_deref(), Some("2.2.1"));
    assert_eq!(report.surfaces[0].export_specifier, "undici");
    assert!(
        report.surfaces[0]
            .evidence
            .as_deref()
            .is_some_and(|evidence| evidence.contains("source_package_import_surface"))
    );
}

#[test]
fn source_backed_import_surface_is_not_accepted_without_unique_version() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "bundle.js",
        Some("const ws = require('ws');".to_string()),
    ));
    let package_sources = [
        PackageSource::external("ws", "8.0.0", "ws", "wrapper.mjs", "export default {};"),
        PackageSource::external(
            "ws",
            "8.18.2",
            "ws",
            "lib/websocket-server.js",
            "export class WebSocketServer {}",
        ),
    ];

    let report = VersionedPackageMatcher::default().match_rows(&rows, &package_sources);

    assert!(report.surfaces.is_empty());
    assert!(
        report
            .audit
            .has(FindingCode::AmbiguousPackageSurfaceVersion)
    );
}

#[test]
fn source_package_import_scan_rejects_unparseable_source_file() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "bundle.js",
        Some("const broken =".to_string()),
    ));

    let error = package_import_names_from_sources(&rows)
        .expect_err("unparseable source import scan should fail");

    assert_eq!(error.source_file_path, "bundle.js");
}
