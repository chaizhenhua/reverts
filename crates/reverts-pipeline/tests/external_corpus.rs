//! Drives the full external bundler corpus through the pipeline and reports
//! coverage. Marked `#[ignore]` because it exercises ~1000 fixture cases and
//! is too slow for the default test loop.
//!
//! Run with:
//!
//! ```text
//! cargo test -p reverts-pipeline --test external_corpus -- --ignored --nocapture
//! ```

use std::collections::{BTreeMap, BTreeSet};

use reverts_analyze::enrich_program;
use reverts_fixtures::external_corpus::{ExternalCase, load_external_cases};
use reverts_input::{
    InputBundle, InputRows, ModuleInput, PackageAttributionStatus, PackageSurfaceInput,
    ProjectInput, SourceFileInput, SymbolInput,
};
use reverts_ir::{ModuleId, split_bare_specifier};
use reverts_model::{CompilerKind, ProgramModel};
use reverts_observe::FindingCode;
use reverts_pipeline::generate_project_from_input;

#[test]
#[ignore = "scans the full external corpus; run with --ignored to enable"]
fn external_corpus_pipeline_coverage_report() {
    let cases = load_external_cases().expect("corpus should load");
    let total = cases.len();

    let mut outcomes: BTreeMap<String, BundlerOutcome> = BTreeMap::new();
    let mut audit_clean = 0usize;
    let mut emit_succeeded = 0usize;
    let mut input_invalid = 0usize;
    let mut artifact_unreadable = 0usize;
    let mut pipeline_failed = 0usize;
    let mut findings_by_code: BTreeMap<FindingCode, usize> = BTreeMap::new();

    for case in &cases {
        let outcome = outcomes
            .entry(case.manifest.expectations.bundler_family.clone())
            .or_default();
        outcome.total += 1;

        let Ok(source) = case.read_artifact_entry() else {
            artifact_unreadable += 1;
            continue;
        };
        let Some(bundle) = build_bundle(case, &source) else {
            input_invalid += 1;
            continue;
        };

        // Detection happens in analyze, *before* any audit gate. Inspect the
        // enriched program independently so per-family detection accuracy
        // reflects what the detector decides, not whether the case happens to
        // emit cleanly.
        let model = ProgramModel::from_input(bundle.clone());
        let enrichment = enrich_program(model);
        let detected = enrichment
            .program
            .compiler_profile()
            .module(ModuleId(1))
            .compiler;
        let expected_kind = expected_compiler_for(&case.manifest.expectations.bundler_family);
        if matches!(expected_kind, Some(kind) if kind == detected) {
            outcome.matched += 1;
        }

        let run = match generate_project_from_input(bundle) {
            Ok(run) => run,
            Err(_) => {
                pipeline_failed += 1;
                continue;
            }
        };

        if run.audit.is_clean() {
            audit_clean += 1;
        } else {
            for finding in run.audit.findings() {
                *findings_by_code.entry(finding.code).or_default() += 1;
            }
        }
        if !run.project.files.is_empty() {
            emit_succeeded += 1;
            if !run.audit.is_clean() {
                continue;
            }
            // When emit happens AND audit is clean, the banner must mirror
            // the analyze-time detection. This ties the two halves of the
            // pipeline together as a regression guard.
            let banner = banner_compiler(&run);
            if banner.is_none() && detected != CompilerKind::Unknown {
                outcome.banner_missing += 1;
            } else if matches!(banner, Some(kind) if kind != detected) {
                outcome.banner_disagreed += 1;
            }
        }
    }

    println!("external corpus coverage:");
    println!("  total cases:        {total}");
    println!("  artifact unread:    {artifact_unreadable}");
    println!("  invalid input:      {input_invalid}");
    println!("  pipeline error:     {pipeline_failed}");
    println!("  emit succeeded:     {emit_succeeded}");
    println!("  audit clean:        {audit_clean}");
    if !findings_by_code.is_empty() {
        println!("  audit findings on non-clean cases:");
        for (code, count) in &findings_by_code {
            println!("    {code:?}: {count}");
        }
    }
    println!("  per-family detection accuracy (analyze stage):");
    for (family, outcome) in &outcomes {
        let percent = if outcome.total == 0 {
            0
        } else {
            outcome.matched * 100 / outcome.total
        };
        println!(
            "    {family:>10}: {} / {} ({percent}%) — emit-stage banner mismatches: missing={} disagreed={}",
            outcome.matched, outcome.total, outcome.banner_missing, outcome.banner_disagreed,
        );
    }
}

#[derive(Debug, Default)]
struct BundlerOutcome {
    total: usize,
    matched: usize,
    banner_missing: usize,
    banner_disagreed: usize,
}

fn build_bundle(case: &ExternalCase, source: &str) -> Option<InputBundle> {
    let mut rows = InputRows::new(ProjectInput::new(1, &case.manifest.id));
    rows.source_files.push(SourceFileInput::new(
        1,
        "bundle.js",
        Some(source.to_string()),
    ));
    rows.modules
        .push(ModuleInput::application(ModuleId(1), "bundle", "src/bundle.ts").with_source_file(1));
    // Synthesize a single nominal symbol so the pipeline has something to plan
    // against; the corpus's own oracle isn't reused here because we only want
    // to measure what the current pipeline observes about the artifact.
    rows.symbols.push(SymbolInput::new(ModuleId(1), "entry"));

    // Paper #2 (Schwartz 2013) iterative-recovery scaffold: probe the input to
    // discover bare package specifiers via the AST extractor, then synthesize a
    // permissive `PackageSurfaceInput` for each so that audit's strict
    // `UnresolvableBareImport` rule does not block exploratory corpus runs.
    // The synthesized surfaces are clearly tagged so a real pipeline run on
    // unmatched bundles can tell them apart from authentic attribution data.
    augment_with_synthesized_surfaces(&mut rows)?;

    InputBundle::from_rows(rows).ok()
}

fn augment_with_synthesized_surfaces(rows: &mut InputRows) -> Option<()> {
    let probe = InputBundle::from_rows(rows.clone()).ok()?;
    let model = ProgramModel::from_input(probe);

    let mut specifiers = BTreeSet::<String>::new();
    for module in model.modules() {
        for specifier in model.graph().import_export().package_imports_for(module.id) {
            specifiers.insert(specifier.to_string());
        }
    }

    for specifier in specifiers {
        let Some((package_name, _)) = split_bare_specifier(&specifier) else {
            continue;
        };
        if !reverts_ir::is_valid_package_name(&package_name) {
            continue;
        }
        rows.package_surfaces.push(PackageSurfaceInput {
            package_name,
            // PackageSurfaceInput requires a non-empty version string when the
            // status is Accepted; the value is opaque to downstream stages.
            package_version: Some("0.0.0-corpus".to_string()),
            export_specifier: specifier,
            status: PackageAttributionStatus::Accepted,
            evidence: Some("auto-synthesized for corpus exploration".to_string()),
        });
    }
    Some(())
}

fn banner_compiler(run: &reverts_pipeline::OutputRun) -> Option<CompilerKind> {
    let emitted = run.project.files.first()?.source.as_str();
    if emitted.contains("// reverts-recovery: webpack") {
        Some(CompilerKind::Webpack)
    } else if emitted.contains("// reverts-recovery: esbuild") {
        Some(CompilerKind::Esbuild)
    } else if emitted.contains("// reverts-recovery: rollup") {
        Some(CompilerKind::Rollup)
    } else if emitted.contains("// reverts-recovery: babel") {
        Some(CompilerKind::Babel)
    } else if emitted.contains("// reverts-recovery: terser") {
        Some(CompilerKind::Terser)
    } else {
        None
    }
}

/// Map a fixture's declared `bundler_family` to the `CompilerKind` we expect
/// our detector to produce when given that fixture's artifact. Some bundlers
/// emit code that is structurally equivalent to another bundler's output and
/// so reuse that detector branch:
///
/// - `bun` and `rolldown` emit esbuild-compatible runtime helpers.
/// - `swc` and `tsc` emit babel-compatible CJS interop.
/// - `vite` emits rollup-style output (rollup is its bundler).
/// - `parcel` ships its own helpers that the detector does not yet recognise.
fn expected_compiler_for(family: &str) -> Option<CompilerKind> {
    match family {
        "webpack" => Some(CompilerKind::Webpack),
        "esbuild" | "bun" | "rolldown" => Some(CompilerKind::Esbuild),
        "rollup" | "vite" => Some(CompilerKind::Rollup),
        "babel" | "swc" | "tsc" => Some(CompilerKind::Babel),
        "parcel" => None,
        _ => None,
    }
}
