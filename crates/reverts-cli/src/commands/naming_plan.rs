//! `naming-plan` command: emit the JSON work list a naming agent consumes —
//! the unnamed (minified, no semantic name) bindings up to a target tier, each
//! carrying the emitted file it lives in so the agent can open and rename it.
//!
//! The work list is the intersection of two views: tier/named status from the
//! graph (`module_naming_facts`) and location from the emitted `symbol_index`
//! (`generate`'s sidecar). Only bindings that actually land in an
//! emitted file are offered — a target with no readable file is not actionable.

use std::collections::BTreeMap;

use reverts_pipeline::{generate_project_from_prepared, prepare_and_enrich};

use crate::args::{NamingPlanArgs, NamingProgressTier};
use crate::commands::module_classify::excluded_module_ids_from_sqlite;
use crate::commands::naming_gates::evidence_tokens;
use crate::commands::naming_progress::{Tier, classify_emitted_entry, emitted_universe};
use crate::commands::symbol_index_io::load_symbol_index;
use crate::errors::{CliRunError, NamingProgressError};
use crate::input_externalization::load_project_bundle_with_package_externalization;

pub(crate) fn run(args: NamingPlanArgs) -> Result<(), CliRunError> {
    let json = naming_plan_json(&args).map_err(CliRunError::NamingProgress)?;
    println!("{json}");
    Ok(())
}

pub fn naming_plan_json(args: &NamingPlanArgs) -> Result<String, NamingProgressError> {
    let excluded = excluded_module_ids_from_sqlite(args.input.as_path(), args.project_id)
        .map_err(NamingProgressError::Classification)?;
    let bundle = load_project_bundle_with_package_externalization(&args.input, args.project_id)
        .map_err(NamingProgressError::LoadInput)?;
    let prepared = prepare_and_enrich(bundle).map_err(NamingProgressError::Pipeline)?;

    // First-party + export view, built before the emit consumes `prepared`. The
    // actionable universe is the emitted symbol index (bindings the agent can
    // open and rename); tier/named come from the same classifier as
    // `naming-progress`, so plan and progress stay coherent.
    let universe = emitted_universe(&prepared.program, &excluded);
    let symbol_index = if let Some(path) = &args.symbol_index {
        load_symbol_index(path.as_path()).map_err(NamingProgressError::ReadSymbolIndex)?
    } else {
        let run =
            generate_project_from_prepared(prepared).map_err(NamingProgressError::Pipeline)?;
        run.symbol_index
    };

    // Keyed by `(module id, emitted file path)`; a `None` module id groups the
    // unnamed bindings of an unmodularized recovered-code file (e.g. the
    // entrypoint island), whose names are accepted through the file-path-keyed
    // `binding-names` channel instead of `symbol-names`.
    let mut by_module: BTreeMap<(Option<u32>, String), Vec<serde_json::Value>> = BTreeMap::new();
    let mut target_count = 0_usize;
    for entry in &symbol_index {
        let Some(detail) = classify_emitted_entry(entry, &universe) else {
            continue;
        };
        if detail.named || !tier_in_scope(detail.tier, args.target_level) {
            continue;
        }
        let slot = by_module
            .entry((
                entry.module_id.map(|module_id| module_id.0),
                entry.file_path.clone(),
            ))
            .or_default();
        slot.push(serde_json::json!({
            "original_name": entry.original_name,
            "emitted_name": entry.emitted_name,
            "tier": tier_str(detail.tier),
            "global_api_surface": detail.global_api_surface,
            "internal_module_surface": detail.internal_module_surface,
            "evidence": naming_evidence(entry, &detail),
            "evidence_tokens": naming_evidence_tokens(entry, &detail),
        }));
        target_count += 1;
    }

    let modules: Vec<serde_json::Value> = by_module
        .into_iter()
        .map(|((module_id, file_path), targets)| {
            serde_json::json!({
                "module_id": module_id,
                "rename_channel": if module_id.is_some() { "symbol-names" } else { "binding-names" },
                "file_path": file_path,
                "targets": targets,
            })
        })
        .collect();

    let plan = serde_json::json!({
        "schema": "reverts.naming_plan.v1",
        "project_id": args.project_id,
        "target_level": target_label(args.target_level),
        "target_count": target_count,
        "module_count": modules.len(),
        "modules": modules,
    });
    Ok(serde_json::to_string_pretty(&plan)
        .expect("serializing a JSON object of plain values is infallible"))
}

fn tier_rank(tier: Tier) -> u8 {
    match tier {
        Tier::PublicSurface => 0,
        Tier::Declarations => 1,
        Tier::Full => 2,
    }
}

fn target_rank(target: NamingProgressTier) -> u8 {
    match target {
        NamingProgressTier::PublicSurface => 0,
        NamingProgressTier::Declarations => 1,
        NamingProgressTier::Full => 2,
    }
}

/// A binding is in scope for a target level when its tier is at or below it
/// (tiers are cumulative: `PublicSurface ⊆ Declarations ⊆ Full`).
fn tier_in_scope(tier: Tier, target: NamingProgressTier) -> bool {
    tier_rank(tier) <= target_rank(target)
}

fn tier_str(tier: Tier) -> &'static str {
    match tier {
        Tier::PublicSurface => "public-surface",
        Tier::Declarations => "declarations",
        Tier::Full => "full",
    }
}

fn target_label(target: NamingProgressTier) -> &'static str {
    match target {
        NamingProgressTier::PublicSurface => "public-surface",
        NamingProgressTier::Declarations => "declarations",
        NamingProgressTier::Full => "full",
    }
}

fn naming_evidence(
    entry: &reverts_pipeline::SymbolIndexEntry,
    detail: &crate::commands::naming_progress::SymbolDetail,
) -> String {
    format!(
        "file_path:{} original:{} emitted:{} tier:{} global_api_surface:{} internal_module_surface:{}",
        entry.file_path,
        entry.original_name,
        entry.emitted_name,
        tier_str(detail.tier),
        detail.global_api_surface,
        detail.internal_module_surface,
    )
}

fn naming_evidence_tokens(
    entry: &reverts_pipeline::SymbolIndexEntry,
    detail: &crate::commands::naming_progress::SymbolDetail,
) -> Vec<String> {
    evidence_tokens(naming_evidence(entry, detail).as_str())
}

#[cfg(test)]
mod tests {
    use super::{tier_in_scope, tier_str};
    use crate::args::NamingProgressTier;
    use crate::commands::naming_progress::Tier;

    #[test]
    fn target_scope_is_cumulative() {
        // public-surface target includes only L1.
        assert!(tier_in_scope(
            Tier::PublicSurface,
            NamingProgressTier::PublicSurface
        ));
        assert!(!tier_in_scope(
            Tier::Declarations,
            NamingProgressTier::PublicSurface
        ));
        assert!(!tier_in_scope(
            Tier::Full,
            NamingProgressTier::PublicSurface
        ));
        // full target includes every tier.
        assert!(tier_in_scope(Tier::PublicSurface, NamingProgressTier::Full));
        assert!(tier_in_scope(Tier::Declarations, NamingProgressTier::Full));
        assert!(tier_in_scope(Tier::Full, NamingProgressTier::Full));
    }

    #[test]
    fn tier_labels_are_stable() {
        assert_eq!(tier_str(Tier::PublicSurface), "public-surface");
        assert_eq!(tier_str(Tier::Declarations), "declarations");
        assert_eq!(tier_str(Tier::Full), "full");
    }
}
