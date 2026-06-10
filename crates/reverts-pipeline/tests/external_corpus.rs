//! Drives the full external bundler corpus through the pipeline and reports
//! coverage. Marked `#[ignore]` because it exercises ~1000 fixture cases and
//! is too slow for the default test loop.
//!
//! Run with:
//!
//! ```text
//! cargo test -p reverts-pipeline --test external_corpus -- --ignored --nocapture
//! ```

use std::collections::BTreeMap;

use reverts_fixtures::external_corpus::{ExternalCase, load_external_cases};
use reverts_input::{
    InputBundle, InputRows, ModuleInput, ProjectInput, SourceFileInput, SymbolInput,
};
use reverts_ir::ModuleId;
use reverts_model::CompilerKind;
use reverts_pipeline::generate_project_from_input;

#[test]
#[ignore = "scans the full external corpus; run with --ignored to enable"]
fn external_corpus_pipeline_coverage_report() {
    let cases = load_external_cases().expect("corpus should load");
    let total = cases.len();

    let mut detection_outcomes: BTreeMap<String, BundlerOutcome> = BTreeMap::new();
    let mut audit_clean = 0usize;
    let mut emit_succeeded = 0usize;
    let mut input_invalid = 0usize;
    let mut artifact_unreadable = 0usize;
    let mut pipeline_failed = 0usize;

    for case in &cases {
        let outcome = detection_outcomes
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

        let run = match generate_project_from_input(bundle) {
            Ok(run) => run,
            Err(_) => {
                pipeline_failed += 1;
                continue;
            }
        };

        if run.audit.is_clean() {
            audit_clean += 1;
        }
        if !run.project.files.is_empty() {
            emit_succeeded += 1;
        }

        // The pipeline currently exposes the compiler decision only via the
        // banner string in the emitted source. Surface it here so we can
        // measure detection accuracy against the case-declared family.
        let detected = detect_compiler_from_emit(&run);
        let expected_kind = expected_compiler_for(&case.manifest.expectations.bundler_family);
        if matches!(detected, Some(kind) if Some(kind) == expected_kind) {
            outcome.matched += 1;
        }
    }

    println!("external corpus coverage:");
    println!("  total cases:        {total}");
    println!("  artifact unread:    {artifact_unreadable}");
    println!("  invalid input:      {input_invalid}");
    println!("  pipeline error:     {pipeline_failed}");
    println!("  emit succeeded:     {emit_succeeded}");
    println!("  audit clean:        {audit_clean}");
    println!("  per-family detection accuracy:");
    for (family, outcome) in &detection_outcomes {
        let percent = if outcome.total == 0 {
            0
        } else {
            outcome.matched * 100 / outcome.total
        };
        println!(
            "    {family:>10}: {} / {} ({percent}%)",
            outcome.matched, outcome.total
        );
    }
}

#[derive(Debug, Default)]
struct BundlerOutcome {
    total: usize,
    matched: usize,
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
    InputBundle::from_rows(rows).ok()
}

fn detect_compiler_from_emit(run: &reverts_pipeline::OutputRun) -> Option<CompilerKind> {
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

fn expected_compiler_for(family: &str) -> Option<CompilerKind> {
    match family {
        "webpack" => Some(CompilerKind::Webpack),
        "esbuild" | "rolldown" => Some(CompilerKind::Esbuild),
        "rollup" | "vite" => Some(CompilerKind::Rollup),
        "babel" | "swc" => Some(CompilerKind::Babel),
        "tsc" => None,
        "bun" | "parcel" => None,
        _ => None,
    }
}
