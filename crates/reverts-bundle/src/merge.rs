use reverts_input::{ModuleInput, SourceSpan};
use reverts_ir::{ByteRange, ModuleId, ModuleKind};
use reverts_observe::{AuditFinding, AuditReport, FindingCode};

use crate::classification::BundleClassification;
use crate::inner_module::InnerModule;

/// Result of merging an extractor classification into upstream
/// `InputRows`. New modules are returned as a separate list so the
/// caller can either inject them into a clone or apply them in-place.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeOutput {
    pub updated_modules: Vec<ModuleInput>,
    pub new_modules: Vec<ModuleInput>,
    pub audit: AuditReport,
}

/// Reconcile one source file's classification with the upstream
/// modules that already point at the same `source_file_id`.
///
/// Rules (spec §4.5):
/// - For each upstream module, pick the extractor `InnerModule` whose
///   `body_span` overlaps the upstream span. The overlap with the
///   largest share wins; ties resolve by smaller `byte_start`. The
///   upstream metadata (`original_name`, `package_name`,
///   `package_version`, `source_file_id`) is preserved and only
///   `source_span` is replaced with the extractor body span.
/// - Upstream modules with no overlapping inner emit
///   `MissingParseableBody` and keep their original span (matcher
///   skips them).
/// - Inner modules with no overlapping upstream become new
///   `ModuleInput` rows (caller assigns final ids).
/// - Runner-up inners on the same upstream span are emitted as new
///   rows with `BundleDetectorAmbiguous` audit findings.
pub fn merge_classification(
    source_file_id: u32,
    upstream_modules: &[ModuleInput],
    classification: &BundleClassification,
    next_synthetic_id: u32,
) -> MergeOutput {
    let mut updated = Vec::new();
    let mut new_modules = Vec::new();
    let mut audit = AuditReport::default();

    let inners: Vec<InnerModule> = match classification {
        BundleClassification::Plain | BundleClassification::Iife(_) => Vec::new(),
        BundleClassification::Marked(meta) => meta.inner_modules.clone(),
    };

    // Per-upstream: use span-overlap ranking only. Stale or missing spans are
    // reported through the audit instead of silently switching match strategy.
    let mut consumed_inner_indices = std::collections::BTreeSet::<usize>::new();
    for upstream in upstream_modules {
        if upstream.source_file_id != Some(source_file_id) {
            updated.push(upstream.clone());
            continue;
        }

        let Some(upstream_span) = upstream.source_span else {
            updated.push(upstream.clone());
            continue;
        };
        let upstream_range = ByteRange::new(upstream_span.byte_start, upstream_span.byte_end);

        let mut scored: Vec<(usize, f64)> = inners
            .iter()
            .enumerate()
            .filter_map(|(i, m)| {
                if consumed_inner_indices.contains(&i) {
                    return None;
                }
                let inter_start = m.body_span.start.max(upstream_range.start);
                let inter_end = m.body_span.end.min(upstream_range.end);
                if inter_end <= inter_start {
                    return None;
                }
                let inter = f64::from(inter_end - inter_start);
                let width = f64::from(upstream_range.end - upstream_range.start);
                let share = if width > 0.0 { inter / width } else { 0.0 };
                Some((i, share))
            })
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    inners[a.0]
                        .body_span
                        .start
                        .cmp(&inners[b.0].body_span.start)
                })
        });

        if let Some((winner_idx, _)) = scored.first().copied() {
            let mut row = upstream.clone();
            let body = inners[winner_idx].body_span;
            row.source_span = Some(SourceSpan {
                byte_start: body.start,
                byte_end: body.end,
            });
            updated.push(row);
            consumed_inner_indices.insert(winner_idx);
            for (runner_idx, _) in scored.iter().skip(1).copied() {
                audit.push(
                    AuditFinding::warning(
                        FindingCode::BundleDetectorAmbiguous,
                        "extractor produced two overlapping inner modules on the same upstream span",
                    )
                    .with_module(upstream.id.0.to_string())
                    .with_binding(inners[runner_idx].virtual_id.clone()),
                );
                consumed_inner_indices.insert(runner_idx);
                // Runners-up are still emitted as new rows (spec §4.5).
                push_new_from_inner(
                    &inners[runner_idx],
                    source_file_id,
                    next_synthetic_id + new_modules.len() as u32,
                    &mut new_modules,
                );
            }
        } else {
            // No overlap.
            updated.push(upstream.clone());
            // Plain / Iife: nothing was extracted, so an upstream with no
            // overlap is expected — don't emit MissingParseableBody.
            // Only Marked classifications produce a list of inners that
            // should have covered every upstream span.
            if !inners.is_empty() {
                audit.push(
                    AuditFinding::error(
                        FindingCode::MissingParseableBody,
                        "no extractor body overlaps this upstream module span",
                    )
                    .with_module(upstream.id.0.to_string()),
                );
            }
        }
    }

    // Unmatched inners become new modules.
    for (i, inner) in inners.iter().enumerate() {
        if consumed_inner_indices.contains(&i) {
            continue;
        }
        push_new_from_inner(
            inner,
            source_file_id,
            next_synthetic_id + new_modules.len() as u32,
            &mut new_modules,
        );
    }

    MergeOutput {
        updated_modules: updated,
        new_modules,
        audit,
    }
}

fn push_new_from_inner(
    inner: &InnerModule,
    source_file_id: u32,
    synthetic_id: u32,
    out: &mut Vec<ModuleInput>,
) {
    let kind = match inner.source_path_hint.as_deref() {
        Some(p) if p.starts_with("node_modules/") => ModuleKind::Package,
        _ => ModuleKind::Application,
    };
    let original_name = inner.virtual_id.clone();
    let semantic_path = inner
        .source_path_hint
        .clone()
        .unwrap_or_else(|| inner.virtual_id.clone());
    let mut row = ModuleInput {
        id: ModuleId(synthetic_id),
        kind,
        original_name,
        semantic_path,
        source_file_id: Some(source_file_id),
        source_span: Some(SourceSpan {
            byte_start: inner.body_span.start,
            byte_end: inner.body_span.end,
        }),
        package_name: None,
        package_version: None,
    };
    if matches!(kind, ModuleKind::Package)
        && let Some(p) = inner.source_path_hint.as_deref()
        && let Some((pkg, _rest)) = parse_node_modules_path(p)
    {
        row.package_name = Some(pkg);
    }
    out.push(row);
}

fn parse_node_modules_path(p: &str) -> Option<(String, String)> {
    let s = p.strip_prefix("node_modules/")?;
    if let Some(slash) = s.find('/') {
        Some((s[..slash].to_string(), s[slash + 1..].to_string()))
    } else {
        Some((s.to_string(), String::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classification::{BundleClassification, MarkedMetadata};
    use crate::inner_module::{BundlerKind, InnerModule};
    use reverts_input::ModuleInput;
    use reverts_ir::ModuleId;

    fn make_upstream(id: u32, file_id: u32, span: (u32, u32), name: &str) -> ModuleInput {
        ModuleInput {
            id: ModuleId(id),
            kind: ModuleKind::Application,
            original_name: name.into(),
            semantic_path: name.into(),
            source_file_id: Some(file_id),
            source_span: Some(SourceSpan {
                byte_start: span.0,
                byte_end: span.1,
            }),
            package_name: None,
            package_version: None,
        }
    }

    fn make_inner(virtual_id: &str, body: (u32, u32), hint: Option<&str>) -> InnerModule {
        InnerModule {
            virtual_id: virtual_id.into(),
            body_span: ByteRange::new(body.0, body.1),
            bundler: BundlerKind::Esbuild,
            source_path_hint: hint.map(str::to_string),
            parent_module_id: ModuleId(0),
        }
    }

    #[test]
    fn overlap_replaces_span_and_preserves_upstream_metadata() {
        let upstream = vec![make_upstream(10, 1, (100, 500), "preserved")];
        let inners = vec![make_inner(
            "esbuild:lib/foo.js",
            (120, 480),
            Some("lib/foo.js"),
        )];
        let classification = BundleClassification::Marked(MarkedMetadata {
            inner_modules: inners,
            detected_by: BundlerKind::Esbuild,
        });
        let result = merge_classification(1, &upstream, &classification, 1000);
        assert_eq!(result.updated_modules.len(), 1);
        let row = &result.updated_modules[0];
        assert_eq!(row.original_name, "preserved");
        assert_eq!(
            row.source_span,
            Some(SourceSpan {
                byte_start: 120,
                byte_end: 480
            })
        );
        assert!(result.new_modules.is_empty());
        assert!(result.audit.is_clean());
    }

    #[test]
    fn upstream_with_no_overlap_emits_missing_parseable_body() {
        let upstream = vec![make_upstream(10, 1, (100, 200), "lonely")];
        let inners = vec![make_inner("esbuild:x", (500, 800), None)];
        let classification = BundleClassification::Marked(MarkedMetadata {
            inner_modules: inners,
            detected_by: BundlerKind::Esbuild,
        });
        let result = merge_classification(1, &upstream, &classification, 1000);
        assert_eq!(result.updated_modules.len(), 1);
        assert_eq!(
            result.updated_modules[0].source_span,
            Some(SourceSpan {
                byte_start: 100,
                byte_end: 200
            })
        );
        assert!(result.audit.has(FindingCode::MissingParseableBody));
        assert_eq!(result.new_modules.len(), 1);
    }

    #[test]
    fn inner_with_no_upstream_becomes_new_module() {
        let upstream: Vec<ModuleInput> = vec![];
        let inners = vec![make_inner(
            "esbuild:node_modules/lodash/index.js",
            (10, 100),
            Some("node_modules/lodash/index.js"),
        )];
        let classification = BundleClassification::Marked(MarkedMetadata {
            inner_modules: inners,
            detected_by: BundlerKind::Esbuild,
        });
        let result = merge_classification(1, &upstream, &classification, 5000);
        assert_eq!(result.new_modules.len(), 1);
        let m = &result.new_modules[0];
        assert_eq!(m.id.0, 5000, "synthetic id uses provided base");
        assert_eq!(m.kind, ModuleKind::Package);
        assert_eq!(m.package_name.as_deref(), Some("lodash"));
        assert_eq!(
            m.source_span,
            Some(SourceSpan {
                byte_start: 10,
                byte_end: 100
            })
        );
    }

    #[test]
    fn overlap_tiebreak_picks_largest_share_then_smaller_start() {
        let upstream = vec![make_upstream(10, 1, (0, 100), "anchor")];
        let inners = vec![
            make_inner("a", (20, 60), None),  // share 0.4
            make_inner("b", (10, 90), None),  // share 0.8
            make_inner("c", (50, 100), None), // share 0.5
        ];
        let classification = BundleClassification::Marked(MarkedMetadata {
            inner_modules: inners,
            detected_by: BundlerKind::Esbuild,
        });
        let result = merge_classification(1, &upstream, &classification, 1000);
        // Inner "b" wins (share 0.8).
        let row = &result.updated_modules[0];
        assert_eq!(
            row.source_span,
            Some(SourceSpan {
                byte_start: 10,
                byte_end: 90
            })
        );
        // Two runners-up generate ambiguous warnings + 2 new modules.
        assert_eq!(
            result
                .audit
                .findings()
                .iter()
                .filter(|f| f.code == FindingCode::BundleDetectorAmbiguous)
                .count(),
            2
        );
        assert_eq!(result.new_modules.len(), 2);
    }

    #[test]
    fn plain_classification_passes_upstream_through() {
        let upstream = vec![make_upstream(10, 1, (0, 100), "preserved")];
        let result = merge_classification(1, &upstream, &BundleClassification::Plain, 1000);
        assert_eq!(result.updated_modules, upstream);
        assert!(result.new_modules.is_empty());
        assert!(result.audit.is_clean());
    }
}
