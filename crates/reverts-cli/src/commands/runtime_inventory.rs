//! `runtime-inventory` command runner, report-printing helpers, and the
//! analysis pipeline that builds the runtime/source counts, the setter
//! migration blocker reports, and the per-line runtime attribution.
//!
//! The CLI dispatcher invokes [`run`], which calls
//! [`runtime_inventory_from_sqlite`] and prints both the per-project and
//! totals summaries via the private `print_runtime_*` helpers.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use reverts_input::{InputBundle, ModuleInput, PackageAttributionInput};
use reverts_ir::{BindingName, ModuleKind};
use reverts_js::{
    ParseGoal, TopLevelStatementFact, TopLevelStatementKind, collect_top_level_statement_facts,
};
use reverts_model::EnrichedProgram;
use reverts_observe::{AuditReport, FindingCode};
use reverts_package::{
    ExternalImportProof, ExternalImportProofKind, is_accepted_external_attribution,
};
use reverts_pipeline::{
    EmittedFile, RuntimeSetterMigrationBindingKey, RuntimeSetterMigrationBindingStatus,
    RuntimeSetterMigrationBlockerReason, RuntimeSetterMigrationBlockerReport,
    generate_project_inventory_from_prepared, prepare_and_enrich,
    runtime_setter_migration_blocker_report_from_prepared,
};
use rusqlite::{Connection, OpenFlags, params};

use crate::args::RuntimeInventoryArgs;
use crate::collect_sqlite_rows;
use crate::errors::{CliRunError, RuntimeInventoryError};
use crate::input_externalization::load_project_bundle_with_package_externalization;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeInventoryOutcome {
    pub projects: Vec<RuntimeInventoryProject>,
    pub totals: RuntimeInventoryCounts,
    pub audit_findings: usize,
    pub skipped_projects: usize,
    pub skipped_source_bytes: u64,
    pub setter_blockers: Option<RuntimeSetterMigrationBlockerReport>,
    pub emitted_setter_blockers: Option<RuntimeSetterMigrationBlockerReport>,
    pub runtime_attribution: Option<RuntimeLineAttributionReport>,
    pub package_source_blockers: Option<PackageSourceBlockerReport>,
    pub finding_clusters: Option<AuditFindingClusterReport>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeInventoryProject {
    pub project_id: u32,
    pub source_bytes: u64,
    pub skipped: bool,
    pub counts: RuntimeInventoryCounts,
    pub audit_findings: usize,
    pub audit_errors: usize,
    pub audit_warnings: usize,
    pub audit_finding_codes: Vec<(String, usize)>,
    pub setter_blockers: Option<RuntimeSetterMigrationBlockerReport>,
    pub emitted_setter_blockers: Option<RuntimeSetterMigrationBlockerReport>,
    pub runtime_attribution: Option<RuntimeLineAttributionReport>,
    pub package_source_blockers: Option<PackageSourceBlockerReport>,
    pub finding_clusters: Option<AuditFindingClusterReport>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeInventoryCounts {
    pub files: usize,
    pub source_lines: usize,
    pub runtime_files: usize,
    pub runtime_lines: usize,
    pub runtime_reexport_only_files: usize,
    pub runtime_import_statements: usize,
    pub runtime_reexport_statements: usize,
    pub setter_occurrences: usize,
    pub setter_import_statements: usize,
    pub setter_function_definitions: usize,
    pub reverts_internal_occurrences: usize,
    pub named_import_statements: usize,
    pub named_export_statements: usize,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RuntimeLineAttributionReport {
    pub total_runtime_lines: usize,
    pub attributed_lines: usize,
    pub unattributed_lines: usize,
    pub items: Vec<RuntimeLineAttributionItem>,
    pub by_kind: BTreeMap<String, RuntimeLineAttributionBucket>,
    pub by_package: BTreeMap<String, RuntimeLineAttributionBucket>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeLineAttributionBucket {
    pub items: usize,
    pub lines: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeLineAttributionItem {
    pub path: String,
    pub line_start: usize,
    pub line_end: usize,
    pub lines: usize,
    pub kind: String,
    pub binding: String,
    pub package: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PackageSourceBlockerReport {
    pub source_package_files: usize,
    pub source_package_bytes: usize,
    pub items: Vec<PackageSourceBlockerItem>,
    pub by_reason: BTreeMap<String, PackageSourceBlockerBucket>,
    pub by_package: BTreeMap<String, PackageSourceBlockerBucket>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PackageSourceBlockerBucket {
    pub files: usize,
    pub bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageSourceBlockerItem {
    pub module_id: u32,
    pub path: String,
    pub package: String,
    pub version: String,
    pub bytes: usize,
    pub reason: String,
    pub detail: String,
}

/// Diagnostic clustering of audit findings (MissingDefinition,
/// DuplicateTopLevelBinding, UnresolvableBareImport, ...) back to the module
/// and symbol-closure facts that produced them. The goal is to turn thousands
/// of scattered findings into a handful of actionable categories so the fix
/// can be made in the graph/planner rather than via post-write string repair.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AuditFindingClusterReport {
    pub total_findings: usize,
    pub items: Vec<AuditFindingClusterItem>,
    pub by_category: BTreeMap<String, usize>,
    pub by_code: BTreeMap<String, usize>,
    pub by_module: BTreeMap<String, usize>,
}

/// One enriched audit finding: the raw finding joined with the owning module's
/// metadata and the cross-module owner candidate for its free binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditFindingClusterItem {
    pub finding_code: String,
    pub module_id: String,
    pub module_original_name: String,
    pub source_file: String,
    pub binding: String,
    pub owner_candidate: String,
    pub is_bundle_source: bool,
    pub is_package_module: bool,
    pub category: String,
}

impl AuditFindingClusterReport {
    fn add(&mut self, other: &Self) {
        self.total_findings += other.total_findings;
        self.items.extend(other.items.iter().cloned());
        merge_count_buckets(&mut self.by_category, &other.by_category);
        merge_count_buckets(&mut self.by_code, &other.by_code);
        merge_count_buckets(&mut self.by_module, &other.by_module);
    }
}

fn merge_count_buckets(into: &mut BTreeMap<String, usize>, from: &BTreeMap<String, usize>) {
    for (key, count) in from {
        *into.entry(key.clone()).or_insert(0) += count;
    }
}

pub(crate) fn audit_finding_cluster_report(
    program: &EnrichedProgram,
    audit: &AuditReport,
) -> AuditFindingClusterReport {
    let model = program.model();
    let def_use = model.graph().def_use();
    let modules_by_id: BTreeMap<u32, &ModuleInput> = model
        .modules()
        .iter()
        .map(|module| (module.id.0, module))
        .collect();
    let source_paths: BTreeMap<u32, &str> = model
        .input()
        .source_files
        .iter()
        .map(|file| (file.id, file.path.as_str()))
        .collect();

    let mut report = AuditFindingClusterReport::default();
    for finding in audit.findings() {
        let code = format!("{:?}", finding.code);
        let binding = finding.binding.clone().unwrap_or_default();
        let module_id_str = finding.module.clone().unwrap_or_default();
        let module = module_id_str
            .parse::<u32>()
            .ok()
            .and_then(|id| modules_by_id.get(&id).copied());

        let module_original_name = module.map_or_else(String::new, |m| m.original_name.clone());
        let source_file = module
            .and_then(|m| m.source_file_id)
            .map(|sid| {
                source_paths
                    .get(&sid)
                    .map_or_else(|| sid.to_string(), |path| (*path).to_string())
            })
            .unwrap_or_default();
        let is_bundle_source = module.is_some_and(|m| m.source_span.is_some());
        let is_package_module = module.is_some_and(|m| m.kind == ModuleKind::Package);

        let owner_modules: Vec<&ModuleInput> = if binding.is_empty() {
            Vec::new()
        } else {
            def_use
                .modules_defining(&BindingName::new(binding.clone()))
                .into_iter()
                .filter(|owner| module.map(|m| m.id) != Some(*owner))
                .filter_map(|owner| modules_by_id.get(&owner.0).copied())
                .collect()
        };
        let owner_candidate = if owner_modules.is_empty() {
            "<none>".to_string()
        } else {
            owner_modules
                .iter()
                .map(|m| m.semantic_path.clone())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let category = classify_audit_finding(finding.code, &binding, owner_modules.len());

        report.items.push(AuditFindingClusterItem {
            finding_code: code.clone(),
            module_id: module_id_str,
            module_original_name,
            source_file,
            binding,
            owner_candidate,
            is_bundle_source,
            is_package_module,
            category: category.clone(),
        });
        *report.by_category.entry(category).or_insert(0) += 1;
        *report.by_code.entry(code).or_insert(0) += 1;
        if let Some(module) = module {
            *report
                .by_module
                .entry(module.semantic_path.clone())
                .or_insert(0) += 1;
        }
    }
    report.total_findings = report.items.len();
    report
}

/// Bucket a finding into an actionable category. The MissingDefinition axis is
/// owner-status + binding-shape (so minified `e`/`t`/`n` free variables — the
/// bundle-wrapper-parameter class — separate from real cross-module owners).
fn classify_audit_finding(code: FindingCode, binding: &str, owner_count: usize) -> String {
    match code {
        FindingCode::MissingDefinition => {
            classify_missing_definition(binding, owner_count).to_string()
        }
        FindingCode::DuplicateTopLevelBinding => "duplicate-binding".to_string(),
        FindingCode::UnresolvableBareImport => "bare-import".to_string(),
        other => format!("{other:?}"),
    }
}

fn classify_missing_definition(binding: &str, owner_count: usize) -> &'static str {
    if binding.is_empty() {
        "no-binding"
    } else if looks_minified(binding) {
        "minified-free-var"
    } else if owner_count == 1 {
        "cross-module-owner"
    } else if owner_count > 1 {
        "cross-module-owner-ambiguous"
    } else {
        "unresolved-symbol"
    }
}

/// Heuristic for a minified identifier — a diagnostic proxy for bundle-wrapper
/// parameters and other closure-scoped names that got sliced out of their
/// defining scope. esbuild/webpack emit dense base-N names up to ~4 chars
/// (`e`, `GPi`, `P7e`, `K3A`, `ZIA`, `Zut`, `zlt`), so any identifier-shaped
/// name of four chars or fewer counts; at five chars a high-entropy signal (a
/// digit, or inner-string caps mixed with lowercase) is required. Real short
/// globals like `URL`/`Map`/`Set` are filtered upstream by the ambient
/// allowlist before reaching a finding, so they do not arrive here.
fn looks_minified(binding: &str) -> bool {
    let len = binding.chars().count();
    if !is_identifier_shaped(binding) {
        return false;
    }
    if len <= 4 {
        return true;
    }
    if len > 5 {
        return false;
    }
    let has_digit = binding.chars().any(|c| c.is_ascii_digit());
    let has_lower = binding.chars().any(|c| c.is_ascii_lowercase());
    let has_inner_upper = binding.chars().skip(1).any(|c| c.is_ascii_uppercase());
    has_digit || (has_inner_upper && has_lower)
}

fn is_identifier_shaped(binding: &str) -> bool {
    !binding.is_empty()
        && binding.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && binding
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

impl PackageSourceBlockerReport {
    fn add(&mut self, other: &Self) {
        self.source_package_files += other.source_package_files;
        self.source_package_bytes += other.source_package_bytes;
        self.items.extend(other.items.iter().cloned());
        merge_package_source_blocker_buckets(&mut self.by_reason, &other.by_reason);
        merge_package_source_blocker_buckets(&mut self.by_package, &other.by_package);
        sort_package_source_blocker_items(&mut self.items);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RuntimeInventoryProjectSelection {
    pub(crate) project_id: u32,
    pub(crate) source_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeSourceSpanOwner {
    pub(crate) byte_start: u32,
    pub(crate) byte_end: u32,
    pub(crate) label: String,
}

/// Read-only diagnostic: build the AST-derived module init-dependency graph over
/// the emitted modules and report how the cycle structure refines from the raw
/// import graph (over-approximate) down to the init-time data-read core (the
/// genuine irreducible cyclic-init dependencies). No code is changed; this only
/// surfaces what [`ModuleInitGraph`] sees, e.g. for de-lazification analysis.
fn print_init_cycle_report(project_id: u32, files: &[EmittedFile]) {
    use reverts_graph::{InitEdgeFilter, ModuleInitGraph};
    let graph = ModuleInitGraph::from_emitted_modules(
        files
            .iter()
            .map(|file| (file.path.clone(), file.source.as_str())),
    );
    let largest = |sccs: &[Vec<usize>]| sccs.iter().map(Vec::len).max().unwrap_or(0);
    println!(
        "  init_cycles project {project_id}: modules={}, import_edges={}, init_call_edges={}, init_read_edges={}",
        graph.node_count(),
        graph.import_edge_count(),
        graph.edge_count(InitEdgeFilter::CallOnly),
        graph.edge_count(InitEdgeFilter::ReadOnly),
    );
    println!(
        "    largest SCC: import={}, init_all={}, init_read={}",
        largest(&graph.import_strongly_connected_components()),
        largest(&graph.strongly_connected_components(InitEdgeFilter::All)),
        largest(&graph.strongly_connected_components(InitEdgeFilter::ReadOnly)),
    );
    println!(
        "    cyclic modules: import={}, init_all={}, init_read={} (the init_read core is the irreducible cyclic-init dependency)",
        graph.import_cyclic_modules().len(),
        graph.cyclic_modules(InitEdgeFilter::All).len(),
        graph.cyclic_modules(InitEdgeFilter::ReadOnly).len(),
    );
}

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
        if let Some(report) = &project.package_source_blockers {
            print_package_source_blocker_report("  package_source_blockers", report);
        }
        if let Some(report) = &project.finding_clusters {
            print_audit_finding_cluster_report("  finding_clusters", report);
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
    let print_total_details = outcome.projects.len() != 1;
    if print_total_details && let Some(report) = &outcome.setter_blockers {
        print_runtime_setter_blocker_report("total setter_blockers", report);
    }
    if print_total_details && let Some(report) = &outcome.emitted_setter_blockers {
        print_runtime_setter_blocker_report("total emitted_setter_blockers", report);
    }
    if print_total_details && let Some(report) = &outcome.runtime_attribution {
        print_runtime_line_attribution_report("total runtime_line_attribution", report);
    }
    if print_total_details && let Some(report) = &outcome.package_source_blockers {
        print_package_source_blocker_report("total package_source_blockers", report);
    }
    if print_total_details && let Some(report) = &outcome.finding_clusters {
        print_audit_finding_cluster_report("total finding_clusters", report);
    }
    Ok(())
}

/// Print the finding-cluster diagnostic: the by-code and by-category rollups
/// (the actionable clustering) plus the heaviest modules. Per-finding rows are
/// gated behind `REVERTS_FINDING_CLUSTER_ROWS=tsv` to keep default output to
/// the buckets rather than thousands of scattered findings.
fn print_audit_finding_cluster_report(label: &str, report: &AuditFindingClusterReport) {
    println!("{label}: total_findings={}", report.total_findings);
    print_count_bucket("  by_code", &report.by_code, usize::MAX);
    print_count_bucket("  by_category", &report.by_category, usize::MAX);
    print_count_bucket("  by_module", &report.by_module, 20);
    if std::env::var_os("REVERTS_FINDING_CLUSTER_ROWS").is_some() {
        println!(
            "  rows: finding_code\tmodule_id\tmodule_original_name\tsource_file\tbinding\towner_candidate\tis_bundle_source\tis_package_module\tcategory"
        );
        for item in &report.items {
            println!(
                "  row\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                item.finding_code,
                item.module_id,
                item.module_original_name,
                item.source_file,
                item.binding,
                item.owner_candidate,
                item.is_bundle_source,
                item.is_package_module,
                item.category,
            );
        }
    }
}

/// Print a `name=count` bucket map sorted by descending count, keeping at most
/// `limit` entries (use `usize::MAX` for all).
fn print_count_bucket(label: &str, bucket: &BTreeMap<String, usize>, limit: usize) {
    if bucket.is_empty() {
        return;
    }
    let mut entries: Vec<(&String, &usize)> = bucket.iter().collect();
    entries.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let shown: Vec<String> = entries
        .iter()
        .take(limit)
        .map(|(name, count)| format!("{name}={count}"))
        .collect();
    println!("{label}: [{}]", shown.join(", "));
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
                    RuntimeSetterMigrationBindingStatus::Blocked {
                        reason: blocked_reason,
                        ..
                    } if *blocked_reason == reason => {
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

fn print_package_source_blocker_report(label: &str, report: &PackageSourceBlockerReport) {
    println!(
        "{label}: files={}, bytes={}",
        report.source_package_files, report.source_package_bytes
    );
    let mut reasons = report
        .by_reason
        .iter()
        .map(|(reason, bucket)| (reason, *bucket))
        .collect::<Vec<_>>();
    reasons.sort_by(|(left_reason, left), (right_reason, right)| {
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| right.files.cmp(&left.files))
            .then_with(|| left_reason.cmp(right_reason))
    });
    for (reason, bucket) in reasons.iter().take(12) {
        println!(
            "  reason {}: files={}, bytes={}",
            reason, bucket.files, bucket.bytes
        );
    }
    let mut packages = report
        .by_package
        .iter()
        .map(|(package, bucket)| (package, *bucket))
        .collect::<Vec<_>>();
    packages.sort_by(|(left_package, left), (right_package, right)| {
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| right.files.cmp(&left.files))
            .then_with(|| left_package.cmp(right_package))
    });
    for (package, bucket) in packages.iter().take(12) {
        println!(
            "  package {}: files={}, bytes={}",
            package, bucket.files, bucket.bytes
        );
    }
    for item in report.items.iter().take(20) {
        println!(
            "  top module={} package={} version={} reason={} bytes={} path={} detail={}",
            item.module_id,
            item.package,
            item.version,
            item.reason,
            item.bytes,
            item.path,
            item.detail
        );
    }
}

pub fn runtime_inventory_from_sqlite(
    args: &RuntimeInventoryArgs,
) -> Result<RuntimeInventoryOutcome, RuntimeInventoryError> {
    let selections = runtime_inventory_project_selections(args)?;
    let mut projects = Vec::with_capacity(selections.len());
    let mut totals = RuntimeInventoryCounts::default();
    let mut audit_findings = 0usize;
    let mut skipped_projects = 0usize;
    let mut skipped_source_bytes = 0u64;
    let mut setter_blockers = args
        .setter_blockers
        .then(RuntimeSetterMigrationBlockerReport::default);
    let mut emitted_setter_blockers = args
        .setter_blockers
        .then(RuntimeSetterMigrationBlockerReport::default);
    let mut runtime_attribution = args
        .runtime_attribution
        .then(RuntimeLineAttributionReport::default);
    let mut package_source_blockers = args
        .package_source_blockers
        .then(PackageSourceBlockerReport::default);
    let mut finding_clusters = args
        .finding_clusters
        .then(AuditFindingClusterReport::default);
    let timing_enabled = std::env::var_os("REVERTS_RUNTIME_INVENTORY_TIMING").is_some();

    for selection in selections {
        let timing_started = std::time::Instant::now();
        let mut timing_last = timing_started;
        macro_rules! mark_timing {
            ($label:literal) => {
                if timing_enabled {
                    let now = std::time::Instant::now();
                    eprintln!(
                        "runtime-inventory timing: project={} {} stage={:.3}s total={:.3}s",
                        selection.project_id,
                        $label,
                        now.duration_since(timing_last).as_secs_f64(),
                        now.duration_since(timing_started).as_secs_f64()
                    );
                    timing_last = now;
                }
            };
        }
        if args
            .max_source_bytes
            .is_some_and(|max_source_bytes| selection.source_bytes > max_source_bytes)
        {
            skipped_projects += 1;
            skipped_source_bytes += selection.source_bytes;
            projects.push(RuntimeInventoryProject {
                project_id: selection.project_id,
                source_bytes: selection.source_bytes,
                skipped: true,
                counts: RuntimeInventoryCounts::default(),
                audit_findings: 0,
                audit_errors: 0,
                audit_warnings: 0,
                audit_finding_codes: Vec::new(),
                setter_blockers: None,
                emitted_setter_blockers: None,
                runtime_attribution: None,
                package_source_blockers: None,
                finding_clusters: None,
            });
            continue;
        }

        let input = load_project_bundle_with_package_externalization(
            args.input.as_path(),
            selection.project_id,
        )
        .map_err(RuntimeInventoryError::LoadInput)?;
        let source_blocker_input = args.package_source_blockers.then(|| input.clone());
        mark_timing!("load_input");
        let prepared = prepare_and_enrich(input).map_err(RuntimeInventoryError::Pipeline)?;
        mark_timing!("prepare_and_enrich");
        let runtime_package_ownership = args
            .runtime_attribution
            .then(|| runtime_package_ownership_by_binding(&prepared.program));
        mark_timing!("runtime_package_ownership");
        let project_setter_blockers = args
            .setter_blockers
            .then(|| runtime_setter_migration_blocker_report_from_prepared(&prepared));
        mark_timing!("setter_blockers");
        if let (Some(total), Some(project_report)) =
            (setter_blockers.as_mut(), project_setter_blockers.as_ref())
        {
            total.add(project_report);
        }
        // The cluster report joins audit findings (only assembled on `run`,
        // post-generate) with the enriched program's def-use owner facts.
        // Clone the program ahead of the move into `generate` so both halves
        // are available; gated on the opt-in flag to avoid the cost otherwise.
        let cluster_program = args.finding_clusters.then(|| prepared.program.clone());
        let run = generate_project_inventory_from_prepared(prepared)
            .map_err(RuntimeInventoryError::Pipeline)?;
        mark_timing!("generate_project_inventory");
        if args.init_cycles {
            print_init_cycle_report(selection.project_id, &run.project.files);
        }
        let project_finding_clusters = cluster_program
            .as_ref()
            .map(|program| audit_finding_cluster_report(program, &run.audit));
        mark_timing!("finding_clusters");
        if let (Some(total), Some(project_report)) =
            (finding_clusters.as_mut(), project_finding_clusters.as_ref())
        {
            total.add(project_report);
        }
        let runtime_facts_by_path = args
            .runtime_attribution
            .then(|| runtime_top_level_statement_facts_by_path(&run.project.files));
        mark_timing!("runtime_facts");
        let counts = runtime_facts_by_path.as_ref().map_or_else(
            || runtime_inventory_counts_from_files(&run.project.files),
            |facts_by_path| {
                runtime_inventory_counts_from_files_with_runtime_facts(
                    &run.project.files,
                    facts_by_path,
                )
            },
        );
        mark_timing!("counts");
        let project_runtime_attribution = runtime_package_ownership.as_ref().map(|ownership| {
            runtime_line_attribution_from_files_with_facts(
                &run.project.files,
                ownership,
                runtime_facts_by_path.as_ref(),
            )
        });
        mark_timing!("runtime_attribution");
        if let (Some(total), Some(project_report)) = (
            runtime_attribution.as_mut(),
            project_runtime_attribution.as_ref(),
        ) {
            total.add(project_report);
        }
        let project_package_source_blockers = source_blocker_input
            .as_ref()
            .map(|input| package_source_blocker_report_from_files(input, &run.project.files));
        mark_timing!("package_source_blockers");
        if let (Some(total), Some(project_report)) = (
            package_source_blockers.as_mut(),
            project_package_source_blockers.as_ref(),
        ) {
            total.add(project_report);
        }
        let project_emitted_setter_blockers = project_setter_blockers
            .as_ref()
            .map(|report| runtime_emitted_setter_blockers_from_files(&run.project.files, report));
        mark_timing!("emitted_setter_blockers");
        if timing_enabled {
            let _ = timing_last;
        }
        if let (Some(total), Some(project_report)) = (
            emitted_setter_blockers.as_mut(),
            project_emitted_setter_blockers.as_ref(),
        ) {
            total.add(project_report);
        }
        let project_audit_findings = run.audit.findings().len();
        let project_audit_errors = run.audit.error_count();
        let project_audit_warnings = run.audit.warning_count();
        let mut finding_code_counts: BTreeMap<String, usize> = BTreeMap::new();
        for finding in run.audit.findings() {
            *finding_code_counts
                .entry(format!("{:?}", finding.code))
                .or_insert(0) += 1;
        }
        let mut audit_finding_codes: Vec<(String, usize)> =
            finding_code_counts.into_iter().collect();
        audit_finding_codes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        if project_audit_findings > 0
            && project_audit_findings <= 20
            && std::env::var_os("REVERTS_RUNTIME_INVENTORY_FINDINGS").is_some()
        {
            for finding in run.audit.findings() {
                eprintln!(
                    "  finding {:?} [{:?}] module={:?} binding={:?} :: {}",
                    finding.severity,
                    finding.code,
                    finding.module,
                    finding.binding,
                    finding.message
                );
            }
        }
        totals.add(counts);
        audit_findings += project_audit_findings;
        projects.push(RuntimeInventoryProject {
            project_id: selection.project_id,
            source_bytes: selection.source_bytes,
            skipped: false,
            counts,
            audit_findings: project_audit_findings,
            audit_errors: project_audit_errors,
            audit_warnings: project_audit_warnings,
            audit_finding_codes,
            setter_blockers: project_setter_blockers,
            emitted_setter_blockers: project_emitted_setter_blockers,
            runtime_attribution: project_runtime_attribution,
            package_source_blockers: project_package_source_blockers,
            finding_clusters: project_finding_clusters,
        });
    }

    Ok(RuntimeInventoryOutcome {
        projects,
        totals,
        audit_findings,
        skipped_projects,
        skipped_source_bytes,
        setter_blockers,
        emitted_setter_blockers,
        runtime_attribution,
        package_source_blockers,
        finding_clusters,
    })
}

pub(crate) fn package_source_blocker_report_from_files(
    input: &InputBundle,
    files: &[EmittedFile],
) -> PackageSourceBlockerReport {
    let files_by_path = files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<BTreeMap<_, _>>();
    let attributions_by_module = input
        .package_attributions
        .iter()
        .filter(|attribution| is_accepted_external_attribution(attribution))
        .map(|attribution| (attribution.module_id, attribution))
        .collect::<BTreeMap<_, _>>();
    let mut report = PackageSourceBlockerReport::default();
    for module in input
        .modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
    {
        let Some(file) = files_by_path.get(module.semantic_path.as_str()) else {
            continue;
        };
        if emitted_file_is_external_adapter(file) {
            continue;
        }
        let package = module
            .package_name
            .clone()
            .unwrap_or_else(|| "<unknown>".to_string());
        let version = module.package_version.clone().unwrap_or_default();
        let attribution = attributions_by_module.get(&module.id).copied();
        let (reason, detail) = package_source_blocker_reason(attribution, file.source.as_str());
        let bytes = file.source.len();
        report.source_package_files += 1;
        report.source_package_bytes += bytes;
        add_package_source_blocker_bucket(&mut report.by_reason, reason.as_str(), bytes);
        add_package_source_blocker_bucket(
            &mut report.by_package,
            package_source_blocker_package_key(package.as_str(), version.as_str()).as_str(),
            bytes,
        );
        report.items.push(PackageSourceBlockerItem {
            module_id: module.id.0,
            path: file.path.clone(),
            package,
            version,
            bytes,
            reason,
            detail,
        });
    }
    sort_package_source_blocker_items(&mut report.items);
    report
}

fn package_source_blocker_reason(
    attribution: Option<&PackageAttributionInput>,
    source: &str,
) -> (String, String) {
    let Some(attribution) = attribution else {
        return (
            "no_external_attribution".to_string(),
            "package module has no accepted external attribution".to_string(),
        );
    };
    if package_source_blocker_has_worker_asset_hint(attribution) {
        return (
            "worker_asset".to_string(),
            attribution
                .export_specifier
                .clone()
                .or_else(|| attribution.resolved_file.clone())
                .unwrap_or_default(),
        );
    }
    let Some(resolved_file) = attribution.resolved_file.as_deref() else {
        return (
            "missing_source_equivalence_proof".to_string(),
            attribution.export_specifier.clone().unwrap_or_default(),
        );
    };
    if package_source_blocker_has_commonjs_named_exports(source)
        && !ExternalImportProof::parse(resolved_file).is_export_member_proof()
    {
        return (
            "commonjs_named_exports_need_member_proof".to_string(),
            resolved_file.to_string(),
        );
    }
    if !resolved_file.contains(':') {
        return (
            "plain_cache_path_suggestion".to_string(),
            resolved_file.to_string(),
        );
    }
    let proof = ExternalImportProof::parse(resolved_file);
    let reason = match proof.kind() {
        ExternalImportProofKind::CanonicalSubpath => Some("canonical_subpath_suggestion"),
        ExternalImportProofKind::SemanticSource => Some("semantic_source_suggestion"),
        ExternalImportProofKind::DependencyGraphSource => {
            Some("dependency_graph_source_suggestion")
        }
        ExternalImportProofKind::DependencyEdgePath => Some("dependency_edge_path_suggestion"),
        _ => None,
    };
    if let Some(reason) = reason {
        return (reason.to_string(), resolved_file.to_string());
    }
    (
        "source_order_or_runtime_dependency".to_string(),
        resolved_file.to_string(),
    )
}

fn emitted_file_is_external_adapter(file: &EmittedFile) -> bool {
    file.source.contains("import * as external_")
}

fn package_source_blocker_has_worker_asset_hint(attribution: &PackageAttributionInput) -> bool {
    [
        attribution.export_specifier.as_deref(),
        attribution.resolved_file.as_deref(),
        attribution.subpath.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|value| value.to_ascii_lowercase().contains(".worker"))
}

fn package_source_blocker_has_commonjs_named_exports(source: &str) -> bool {
    let compact = source.split_whitespace().collect::<String>();
    compact.contains("exports.") || compact.contains(".exports={")
}

fn package_source_blocker_package_key(package: &str, version: &str) -> String {
    if version.is_empty() {
        package.to_string()
    } else {
        format!("{package}@{version}")
    }
}

fn add_package_source_blocker_bucket(
    buckets: &mut BTreeMap<String, PackageSourceBlockerBucket>,
    key: &str,
    bytes: usize,
) {
    let bucket = buckets.entry(key.to_string()).or_default();
    bucket.files += 1;
    bucket.bytes += bytes;
}

fn merge_package_source_blocker_buckets(
    target: &mut BTreeMap<String, PackageSourceBlockerBucket>,
    source: &BTreeMap<String, PackageSourceBlockerBucket>,
) {
    for (key, bucket) in source {
        let target_bucket = target.entry(key.clone()).or_default();
        target_bucket.files += bucket.files;
        target_bucket.bytes += bucket.bytes;
    }
}

fn sort_package_source_blocker_items(items: &mut [PackageSourceBlockerItem]) {
    items.sort_by(|left, right| {
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| left.package.cmp(&right.package))
            .then_with(|| left.path.cmp(&right.path))
    });
}

pub(crate) fn runtime_emitted_setter_blockers_from_files(
    files: &[EmittedFile],
    report: &RuntimeSetterMigrationBlockerReport,
) -> RuntimeSetterMigrationBlockerReport {
    let mut emitted = RuntimeSetterMigrationBlockerReport::default();
    for file in files {
        let Some(source_file_id) = runtime_helper_source_file_id(file.path.as_str()) else {
            continue;
        };
        for binding in runtime_setter_targets_in_source(file.source.as_str()) {
            emitted.total_bindings += 1;
            let key = RuntimeSetterMigrationBindingKey {
                source_file_id,
                binding: binding.clone(),
            };
            match report.binding_statuses.get(&key).copied() {
                Some(RuntimeSetterMigrationBindingStatus::Accepted) => {
                    emitted.add_accepted(source_file_id, binding);
                }
                Some(RuntimeSetterMigrationBindingStatus::Blocked { reason, sub_reason }) => {
                    emitted.add_reason_with_sub(source_file_id, binding, reason, sub_reason);
                }
                None => emitted.add_reason(
                    source_file_id,
                    binding,
                    RuntimeSetterMigrationBlockerReason::NoDiagnosticStatus,
                ),
            }
        }
    }
    emitted
}

fn runtime_helper_source_file_id(path: &str) -> Option<u32> {
    let filename = Path::new(path).file_name()?.to_str()?;
    let rest = filename.strip_prefix("source-")?;
    let source_id = rest.strip_suffix("-helpers.ts")?;
    source_id.parse().ok()
}

fn runtime_setter_targets_in_source(source: &str) -> Vec<BindingName> {
    // When the emitted runtime helper file can't be re-parsed (rare bundle
    // shapes the TS parser refuses, e.g. JSX comma patterns), return no
    // setters from this source — the file already has an UnparseableOutput
    // audit finding; we don't want to take the whole inventory run down.
    let Ok(facts) = collect_top_level_statement_facts(source, None, ParseGoal::TypeScript) else {
        return Vec::new();
    };
    facts
        .into_iter()
        .filter(|fact| fact.kind == TopLevelStatementKind::Setter)
        .flat_map(|fact| fact.bindings)
        .filter_map(|binding| runtime_setter_target_from_binding(binding.as_str()))
        .collect()
}

fn runtime_setter_target_from_binding(binding: &str) -> Option<BindingName> {
    let target = binding.strip_prefix("__reverts_set_")?;
    (!target.is_empty()).then(|| BindingName::new(target))
}

pub(crate) fn runtime_inventory_project_selections(
    args: &RuntimeInventoryArgs,
) -> Result<Vec<RuntimeInventoryProjectSelection>, RuntimeInventoryError> {
    let connection =
        Connection::open_with_flags(args.input.as_path(), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|source| RuntimeInventoryError::OpenDatabase {
                path: args.input.clone(),
                source,
            })?;

    if let Some(project_id) = args.project_id {
        let selection = connection
            .query_row(
                r"
                SELECT ?1, COALESCE(SUM(sf.file_size), 0)
                FROM project_files pf
                JOIN source_files sf ON sf.id = pf.file_id
                WHERE pf.project_id = ?1
                ",
                params![i64::from(project_id)],
                runtime_inventory_project_selection_from_row,
            )
            .map_err(RuntimeInventoryError::QueryProjects)?;
        return Ok(vec![selection]);
    }

    let order = if args.newest { "DESC" } else { "ASC" };
    let sql = if args.limit.is_some() {
        format!(
            r"
            SELECT p.id, COALESCE(SUM(sf.file_size), 0)
            FROM projects p
            LEFT JOIN project_files pf ON pf.project_id = p.id
            LEFT JOIN source_files sf ON sf.id = pf.file_id
            GROUP BY p.id
            ORDER BY p.id {order}
            LIMIT ?1
            "
        )
    } else {
        format!(
            r"
            SELECT p.id, COALESCE(SUM(sf.file_size), 0)
            FROM projects p
            LEFT JOIN project_files pf ON pf.project_id = p.id
            LEFT JOIN source_files sf ON sf.id = pf.file_id
            GROUP BY p.id
            ORDER BY p.id {order}
            "
        )
    };
    let mut statement = connection
        .prepare(sql.as_str())
        .map_err(RuntimeInventoryError::QueryProjects)?;
    if let Some(limit) = args.limit {
        let rows = statement
            .query_map(
                [i64::from(limit)],
                runtime_inventory_project_selection_from_row,
            )
            .map_err(RuntimeInventoryError::QueryProjects)?;
        collect_sqlite_rows(rows).map_err(RuntimeInventoryError::QueryProjects)
    } else {
        let rows = statement
            .query_map([], runtime_inventory_project_selection_from_row)
            .map_err(RuntimeInventoryError::QueryProjects)?;
        collect_sqlite_rows(rows).map_err(RuntimeInventoryError::QueryProjects)
    }
}

fn runtime_inventory_project_selection_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<RuntimeInventoryProjectSelection> {
    Ok(RuntimeInventoryProjectSelection {
        project_id: row.get(0)?,
        source_bytes: row.get(1)?,
    })
}

pub(crate) fn runtime_inventory_counts_from_files(files: &[EmittedFile]) -> RuntimeInventoryCounts {
    runtime_inventory_counts_from_files_with_runtime_facts(files, &BTreeMap::new())
}

fn runtime_inventory_counts_from_files_with_runtime_facts(
    files: &[EmittedFile],
    runtime_facts_by_path: &BTreeMap<String, Vec<TopLevelStatementFact>>,
) -> RuntimeInventoryCounts {
    let mut counts = RuntimeInventoryCounts {
        files: files.len(),
        ..Default::default()
    };
    for file in files {
        let is_runtime_file = file.path.starts_with("modules/runtime/");
        counts.source_lines += file.source.lines().count();
        if is_runtime_file {
            counts.runtime_files += 1;
            counts.runtime_lines += file.source.lines().count();
            if is_runtime_reexport_only_source(file.source.as_str()) {
                counts.runtime_reexport_only_files += 1;
            }
        }
        counts.setter_occurrences += count_substring(file.source.as_str(), "__reverts_set_");
        counts.reverts_internal_occurrences += count_substring(file.source.as_str(), "__reverts_");
        for line in file.source.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("import {") {
                counts.named_import_statements += 1;
            }
            if trimmed.starts_with("export {") {
                counts.named_export_statements += 1;
            }
            if trimmed.starts_with("import ") && line_contains_runtime_path(trimmed) {
                counts.runtime_import_statements += 1;
            }
            if trimmed.starts_with("import ") && trimmed.contains("__reverts_set_") {
                counts.setter_import_statements += 1;
            }
            if is_runtime_file && trimmed.starts_with("export {") && trimmed.contains(" from ") {
                counts.runtime_reexport_statements += 1;
            }
        }
        if is_runtime_file {
            counts.setter_function_definitions +=
                runtime_facts_by_path.get(file.path.as_str()).map_or_else(
                    || runtime_setter_targets_in_source(file.source.as_str()).len(),
                    |facts| runtime_setter_targets_in_facts(facts).len(),
                );
        }
    }
    counts
}

#[cfg(test)]
pub(crate) fn runtime_line_attribution_from_files(
    files: &[EmittedFile],
    package_ownership: &BTreeMap<(u32, String), String>,
) -> RuntimeLineAttributionReport {
    runtime_line_attribution_from_files_with_facts(files, package_ownership, None)
}

fn runtime_line_attribution_from_files_with_facts(
    files: &[EmittedFile],
    package_ownership: &BTreeMap<(u32, String), String>,
    runtime_facts_by_path: Option<&BTreeMap<String, Vec<TopLevelStatementFact>>>,
) -> RuntimeLineAttributionReport {
    let mut report = RuntimeLineAttributionReport::default();
    for file in files {
        if !file.path.starts_with("modules/runtime/") {
            continue;
        }
        let runtime_lines = file.source.lines().count();
        report.total_runtime_lines += runtime_lines;
        let mut covered_lines = BTreeSet::<usize>::new();
        let line_starts = line_start_offsets(file.source.as_str());
        let facts = runtime_facts_by_path
            .and_then(|facts_by_path| facts_by_path.get(file.path.as_str()).cloned())
            .unwrap_or_else(|| {
                collect_top_level_statement_facts(
                    file.source.as_str(),
                    Some(Path::new(file.path.as_str())),
                    ParseGoal::TypeScript,
                )
                .expect("runtime line attribution requires parseable generated TypeScript source")
            });
        for fact in facts {
            let line_start = line_number_for_offset(&line_starts, fact.byte_start as usize);
            let line_end = line_number_for_statement_end(&line_starts, fact.byte_end as usize);
            if line_end < line_start {
                continue;
            }
            for line in line_start..=line_end {
                covered_lines.insert(line);
            }
            let lines = line_end - line_start + 1;
            let binding = runtime_attribution_binding_label(&fact.bindings);
            let kind = fact.kind.as_str().to_string();
            let package = runtime_attribution_package_label(
                runtime_helper_source_file_id(file.path.as_str()),
                &fact.bindings,
                package_ownership,
            );
            let bucket = report.by_kind.entry(kind.clone()).or_default();
            bucket.items += 1;
            bucket.lines += lines;
            let package_bucket = report.by_package.entry(package.clone()).or_default();
            package_bucket.items += 1;
            package_bucket.lines += lines;
            report.items.push(RuntimeLineAttributionItem {
                path: file.path.clone(),
                line_start,
                line_end,
                lines,
                kind,
                binding,
                package,
            });
        }
        report.attributed_lines += covered_lines.len();
        report.unattributed_lines += runtime_lines.saturating_sub(covered_lines.len());
    }
    report
}

fn runtime_top_level_statement_facts_by_path(
    files: &[EmittedFile],
) -> BTreeMap<String, Vec<TopLevelStatementFact>> {
    files
        .iter()
        .filter(|file| file.path.starts_with("modules/runtime/"))
        .map(|file| {
            let facts = collect_top_level_statement_facts(
                file.source.as_str(),
                Some(Path::new(file.path.as_str())),
                ParseGoal::TypeScript,
            )
            .expect("runtime inventory attribution requires parseable generated TypeScript source");
            (file.path.clone(), facts)
        })
        .collect()
}

fn runtime_setter_targets_in_facts(facts: &[TopLevelStatementFact]) -> Vec<BindingName> {
    facts
        .iter()
        .filter(|fact| fact.kind == TopLevelStatementKind::Setter)
        .flat_map(|fact| fact.bindings.iter())
        .filter_map(|binding| runtime_setter_target_from_binding(binding.as_str()))
        .collect()
}

fn runtime_attribution_package_label(
    source_file_id: Option<u32>,
    bindings: &[String],
    package_ownership: &BTreeMap<(u32, String), String>,
) -> String {
    let Some(source_file_id) = source_file_id else {
        return "<runtime-glue>".to_string();
    };
    let Some(primary_binding) = bindings.first() else {
        return "<runtime-glue>".to_string();
    };
    let target_binding = primary_binding
        .strip_prefix("__reverts_set_")
        .unwrap_or(primary_binding);
    package_ownership
        .get(&(source_file_id, target_binding.to_string()))
        .cloned()
        .unwrap_or_else(|| "<unknown>".to_string())
}

fn runtime_package_ownership_by_binding(
    program: &EnrichedProgram,
) -> BTreeMap<(u32, String), String> {
    let input = program.model().input();
    let source_owners = runtime_source_span_owners(input);
    let original_name_owners = runtime_original_name_owners_by_binding(&input.modules);
    let mut consumer_owners = BTreeMap::<(u32, String), BTreeSet<String>>::new();
    for module in program.model().modules() {
        let owner = runtime_module_owner_label(module);
        for import in program.model().graph().runtime_imports_for(module.id) {
            consumer_owners
                .entry((import.source_file_id, import.binding.as_str().to_string()))
                .or_default()
                .insert(owner.clone());
        }
    }
    let mut ownership = BTreeMap::<(u32, String), String>::new();
    for (key, owners) in &original_name_owners {
        ownership.insert(key.clone(), runtime_consumer_owner_label(owners));
    }
    for (source_file_id, prelude) in program.model().graph().runtime_preludes() {
        let Some(owners) = source_owners.get(source_file_id) else {
            for binding in prelude.snippets.keys() {
                let key = (*source_file_id, binding.as_str().to_string());
                let owner = consumer_owners
                    .get(&key)
                    .map(runtime_consumer_owner_label)
                    .or_else(|| {
                        original_name_owners
                            .get(&key)
                            .map(runtime_consumer_owner_label)
                    })
                    .unwrap_or_else(|| "<unknown>".to_string());
                ownership.insert(key, owner);
            }
            continue;
        };
        for (binding, snippet) in &prelude.snippets {
            let key = (*source_file_id, binding.as_str().to_string());
            let snippet_byte_end = snippet
                .byte_start
                .saturating_add(u32::try_from(snippet.source.len()).unwrap_or(u32::MAX));
            let owner = consumer_owners
                .get(&key)
                .map(runtime_consumer_owner_label)
                .or_else(|| {
                    original_name_owners
                        .get(&key)
                        .map(runtime_consumer_owner_label)
                })
                .or_else(|| {
                    runtime_source_span_owner_label_for_range(
                        owners,
                        snippet.byte_start,
                        snippet_byte_end,
                    )
                })
                .unwrap_or_else(|| "<unknown>".to_string());
            ownership.insert(key, owner);
        }
    }
    ownership
}

pub(crate) fn runtime_original_name_owners_by_binding(
    modules: &[ModuleInput],
) -> BTreeMap<(u32, String), BTreeSet<String>> {
    let mut owners = BTreeMap::<(u32, String), BTreeSet<String>>::new();
    for module in modules {
        let Some(source_file_id) = module.source_file_id else {
            continue;
        };
        owners
            .entry((source_file_id, module.original_name.clone()))
            .or_default()
            .insert(runtime_module_owner_label(module));
    }
    owners
}

fn runtime_consumer_owner_label(owners: &BTreeSet<String>) -> String {
    match owners.len() {
        0 => "<unknown>".to_string(),
        1 => owners
            .iter()
            .next()
            .cloned()
            .unwrap_or_else(|| "<unknown>".to_string()),
        _ => "<shared>".to_string(),
    }
}

fn runtime_source_span_owners(input: &InputBundle) -> BTreeMap<u32, Vec<RuntimeSourceSpanOwner>> {
    let mut owners = BTreeMap::<u32, Vec<RuntimeSourceSpanOwner>>::new();
    for module in &input.modules {
        let Some(source_file_id) = module.source_file_id else {
            continue;
        };
        let Some(span) = module.source_span else {
            continue;
        };
        owners
            .entry(source_file_id)
            .or_default()
            .push(RuntimeSourceSpanOwner {
                byte_start: span.byte_start,
                byte_end: span.byte_end,
                label: runtime_module_owner_label(module),
            });
    }
    for source_owners in owners.values_mut() {
        source_owners.sort_by_key(|owner| (owner.byte_start, owner.byte_end));
    }
    owners
}

pub(crate) fn runtime_source_span_owner_label_for_range(
    owners: &[RuntimeSourceSpanOwner],
    byte_start: u32,
    byte_end: u32,
) -> Option<String> {
    let mut overlapping = BTreeSet::<String>::new();
    for owner in owners {
        if owner.byte_start < byte_end && byte_start < owner.byte_end {
            overlapping.insert(owner.label.clone());
        }
    }
    if overlapping.is_empty() {
        None
    } else {
        Some(runtime_consumer_owner_label(&overlapping))
    }
}

pub(crate) fn runtime_module_owner_label(module: &ModuleInput) -> String {
    if let Some(package_name) = module
        .package_name
        .as_deref()
        .filter(|name| !name.is_empty())
    {
        return format!(
            "{}@{}",
            package_name,
            module.package_version.as_deref().unwrap_or("unknown")
        );
    }
    match module.kind {
        ModuleKind::Package => "<package>".to_string(),
        ModuleKind::Application => "<application>".to_string(),
        ModuleKind::Builtin => "<builtin>".to_string(),
    }
}

fn runtime_attribution_binding_label(bindings: &[String]) -> String {
    match bindings {
        [] => "<anonymous>".to_string(),
        [binding] => binding.clone(),
        [first, rest @ ..] => format!("{first}+{}", rest.len()),
    }
}

fn line_start_offsets(source: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (index, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(index + 1);
        }
    }
    starts
}

fn line_number_for_offset(line_starts: &[usize], offset: usize) -> usize {
    match line_starts.binary_search(&offset) {
        Ok(index) => index + 1,
        Err(index) => index.max(1),
    }
}

fn line_number_for_statement_end(line_starts: &[usize], end_offset: usize) -> usize {
    line_number_for_offset(line_starts, end_offset.saturating_sub(1))
}

fn is_runtime_reexport_only_source(source: &str) -> bool {
    let mut saw_relevant_line = false;
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        saw_relevant_line = true;
        if !(trimmed.starts_with("export {") && trimmed.contains(" from ")) {
            return false;
        }
    }
    saw_relevant_line
}

fn line_contains_runtime_path(line: &str) -> bool {
    line.contains("/runtime/")
}

fn count_substring(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

impl RuntimeInventoryCounts {
    fn add(&mut self, other: Self) {
        self.files += other.files;
        self.source_lines += other.source_lines;
        self.runtime_files += other.runtime_files;
        self.runtime_lines += other.runtime_lines;
        self.runtime_reexport_only_files += other.runtime_reexport_only_files;
        self.runtime_import_statements += other.runtime_import_statements;
        self.runtime_reexport_statements += other.runtime_reexport_statements;
        self.setter_occurrences += other.setter_occurrences;
        self.setter_import_statements += other.setter_import_statements;
        self.setter_function_definitions += other.setter_function_definitions;
        self.reverts_internal_occurrences += other.reverts_internal_occurrences;
        self.named_import_statements += other.named_import_statements;
        self.named_export_statements += other.named_export_statements;
    }
}

impl RuntimeLineAttributionReport {
    fn add(&mut self, other: &Self) {
        self.total_runtime_lines += other.total_runtime_lines;
        self.attributed_lines += other.attributed_lines;
        self.unattributed_lines += other.unattributed_lines;
        self.items.extend(other.items.iter().cloned());
        for (kind, bucket) in &other.by_kind {
            let target = self.by_kind.entry(kind.clone()).or_default();
            target.items += bucket.items;
            target.lines += bucket.lines;
        }
        for (package, bucket) in &other.by_package {
            let target = self.by_package.entry(package.clone()).or_default();
            target.items += bucket.items;
            target.lines += bucket.lines;
        }
    }
}
