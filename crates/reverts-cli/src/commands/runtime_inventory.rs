//! `runtime-inventory` command runner and report-printing helpers.
//!
//! The actual analysis runs via `runtime_inventory_from_sqlite` (still in
//! `lib.rs` for now — has many helper dependencies that haven't been
//! extracted yet). This module owns presentation: turning a
//! `RuntimeInventoryOutcome` into the human-readable stdout report and
//! sub-printing the blocker / line-attribution sub-reports for both
//! per-project and totals scopes.

use reverts_pipeline::{RuntimeSetterMigrationBindingStatus, RuntimeSetterMigrationBlockerReport};

use crate::args::RuntimeInventoryArgs;
use crate::errors::CliRunError;
use crate::{RuntimeLineAttributionReport, runtime_inventory_from_sqlite};

pub(crate) fn run(args: RuntimeInventoryArgs) -> Result<(), CliRunError> {
    let outcome = runtime_inventory_from_sqlite(&args).map_err(CliRunError::RuntimeInventory)?;
    println!(
        "runtime inventory for {} project(s), {} skipped",
        outcome.projects.len(),
        outcome.skipped_projects
    );
    for project in &outcome.projects {
        println!(
            "project {}: source_bytes={}, skipped={}, files={}, source_lines={}, runtime_files={}, runtime_lines={}, runtime_reexport_only_files={}, runtime_imports={}, runtime_reexports={}, setters={}, setter_imports={}, setter_functions={}, reverts_internal_names={}, named_imports={}, named_exports={}, audit_findings={}",
            project.project_id,
            project.source_bytes,
            project.skipped,
            project.counts.files,
            project.counts.source_lines,
            project.counts.runtime_files,
            project.counts.runtime_lines,
            project.counts.runtime_reexport_only_files,
            project.counts.runtime_import_statements,
            project.counts.runtime_reexport_statements,
            project.counts.setter_occurrences,
            project.counts.setter_import_statements,
            project.counts.setter_function_definitions,
            project.counts.reverts_internal_occurrences,
            project.counts.named_import_statements,
            project.counts.named_export_statements,
            project.audit_findings,
        );
        if project.audit_findings > 0 {
            let top: Vec<String> = project
                .audit_finding_codes
                .iter()
                .take(8)
                .map(|(code, count)| format!("{code}={count}"))
                .collect();
            println!(
                "  audit: errors={}, warnings={}, top=[{}]",
                project.audit_errors,
                project.audit_warnings,
                top.join(", ")
            );
        }
        if let Some(report) = &project.setter_blockers {
            print_runtime_setter_blocker_report("  setter_blockers", report);
        }
        if let Some(report) = &project.emitted_setter_blockers {
            print_runtime_setter_blocker_report("  emitted_setter_blockers", report);
        }
        if let Some(report) = &project.runtime_attribution {
            print_runtime_line_attribution_report("  runtime_line_attribution", report);
        }
    }
    println!(
        "total: files={}, source_lines={}, runtime_files={}, runtime_lines={}, runtime_reexport_only_files={}, runtime_imports={}, runtime_reexports={}, setters={}, setter_imports={}, setter_functions={}, reverts_internal_names={}, named_imports={}, named_exports={}, audit_findings={}, skipped_source_bytes={}",
        outcome.totals.files,
        outcome.totals.source_lines,
        outcome.totals.runtime_files,
        outcome.totals.runtime_lines,
        outcome.totals.runtime_reexport_only_files,
        outcome.totals.runtime_import_statements,
        outcome.totals.runtime_reexport_statements,
        outcome.totals.setter_occurrences,
        outcome.totals.setter_import_statements,
        outcome.totals.setter_function_definitions,
        outcome.totals.reverts_internal_occurrences,
        outcome.totals.named_import_statements,
        outcome.totals.named_export_statements,
        outcome.audit_findings,
        outcome.skipped_source_bytes,
    );
    if let Some(report) = &outcome.setter_blockers {
        print_runtime_setter_blocker_report("total setter_blockers", report);
    }
    if let Some(report) = &outcome.emitted_setter_blockers {
        print_runtime_setter_blocker_report("total emitted_setter_blockers", report);
    }
    if let Some(report) = &outcome.runtime_attribution {
        print_runtime_line_attribution_report("total runtime_line_attribution", report);
    }
    Ok(())
}

fn print_runtime_setter_blocker_report(label: &str, report: &RuntimeSetterMigrationBlockerReport) {
    println!(
        "{label}: total={}, accepted={}, blocked={}",
        report.total_bindings, report.accepted_bindings, report.blocked_bindings
    );
    let mut reasons = report
        .reasons
        .iter()
        .map(|(reason, count)| (*reason, *count))
        .collect::<Vec<_>>();
    reasons.sort_by(|(left_reason, left_count), (right_reason, right_count)| {
        right_count
            .cmp(left_count)
            .then_with(|| left_reason.as_str().cmp(right_reason.as_str()))
    });
    for (reason, count) in reasons {
        println!("  {}={}", reason.as_str(), count);
        let mut sub_reasons = report
            .sub_reasons
            .iter()
            .filter_map(|((r, label), count)| (*r == reason).then_some((*label, *count)))
            .collect::<Vec<_>>();
        sub_reasons.sort_by(|(left_label, left_count), (right_label, right_count)| {
            right_count
                .cmp(left_count)
                .then_with(|| left_label.cmp(right_label))
        });
        for (label, sub_count) in sub_reasons {
            println!("    {label}={sub_count}");
        }
        if std::env::var_os("REVERTS_RUNTIME_BLOCKER_EXAMPLES").is_some() {
            let examples = report
                .binding_statuses
                .iter()
                .filter_map(|(key, status)| match status {
                    RuntimeSetterMigrationBindingStatus::Blocked(blocked_reason)
                        if *blocked_reason == reason =>
                    {
                        Some(format!("{}:{}", key.source_file_id, key.binding.as_str()))
                    }
                    _ => None,
                })
                .take(10)
                .collect::<Vec<_>>();
            if !examples.is_empty() {
                println!("    examples: {}", examples.join(", "));
            }
        }
    }
}

fn print_runtime_line_attribution_report(label: &str, report: &RuntimeLineAttributionReport) {
    println!(
        "{label}: runtime_lines={}, attributed_lines={}, unattributed_lines={}, items={}",
        report.total_runtime_lines,
        report.attributed_lines,
        report.unattributed_lines,
        report.items.len()
    );
    let mut buckets = report
        .by_kind
        .iter()
        .map(|(kind, bucket)| (kind.as_str(), *bucket))
        .collect::<Vec<_>>();
    buckets.sort_by(|(left_kind, left_bucket), (right_kind, right_bucket)| {
        right_bucket
            .lines
            .cmp(&left_bucket.lines)
            .then_with(|| left_kind.cmp(right_kind))
    });
    for (kind, bucket) in buckets {
        println!(
            "  kind {kind}: lines={}, items={}",
            bucket.lines, bucket.items
        );
    }
    let mut package_buckets = report
        .by_package
        .iter()
        .map(|(package, bucket)| (package.as_str(), *bucket))
        .collect::<Vec<_>>();
    package_buckets.sort_by(
        |(left_package, left_bucket), (right_package, right_bucket)| {
            right_bucket
                .lines
                .cmp(&left_bucket.lines)
                .then_with(|| left_package.cmp(right_package))
        },
    );
    for (package, bucket) in package_buckets.iter().take(20) {
        let runtime_pct = if report.total_runtime_lines == 0 {
            0.0
        } else {
            (bucket.lines as f64 * 100.0) / report.total_runtime_lines as f64
        };
        println!(
            "  package {package}: lines={}, pct={runtime_pct:.2}%, items={}",
            bucket.lines, bucket.items
        );
    }
    let mut items = report.items.clone();
    items.sort_by(|left, right| {
        right
            .lines
            .cmp(&left.lines)
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| left.binding.cmp(&right.binding))
    });
    for item in items.iter().take(20) {
        println!(
            "  top {} {} package={} {}:{}-{} lines={}",
            item.kind,
            item.binding,
            item.package,
            item.path,
            item.line_start,
            item.line_end,
            item.lines
        );
    }
}
