//! Attribution precision gate: de-noise grossly OVER-ATTRIBUTED packages by
//! dropping their weak, uncorroborated aggregate-function-shape matches.
//!
//! The aggregate function-signature matcher is promiscuous: a package with many
//! tiny functions (e.g. zod@4's hundreds of validators) coincidentally
//! function-matches hundreds of unrelated bundle modules (cc-2.1.89: zod
//! attributed to 2733 modules, ~1521 of them via
//! `AggregateFunctionSignatureAndStringAnchors` with ZERO string anchors —
//! pure function-shape collisions on first-party code). That noise corrupts
//! classification AND pins genuine third-party modules from being dropped (a
//! noise-attributed module counts as a "kept consumer" in the planner closure).
//!
//! There is no clean PER-MODULE signal separating a genuine package-internal
//! match from function-soup noise (both are aggregate strategy, anchors=0, and
//! assign no public members) — proven by two earlier gate designs that each
//! broke legitimate-recognition unit tests. So the gate keys on a PACKAGE-LEVEL
//! over-attribution signal: it only fires for packages attributed to MANY
//! modules (`MIN_PACKAGE_MODULES_FOR_DENOISE`), where promiscuous over-fire is
//! the only plausible explanation; small/normal packages (and every small unit
//! fixture) are left completely untouched. Within an over-attributed package,
//! suppress a match ONLY when it is weak in every dimension:
//!   - strategy is aggregate function-signature (the promiscuous one);
//!   - ZERO string anchors (no distinctive-string corroboration);
//!   - NOT externalized (never drop an accepted external);
//!   - does NOT assign the package's real public members (not the genuine entry).
//!
//! The package's genuine entry (externalized, or assigns its public members, or
//! carries string anchors, or matched via a non-promiscuous strategy) is kept,
//! so the package stays present — only the function-soup tail is trimmed.

use std::collections::{BTreeMap, BTreeSet};

use reverts_input::InputRows;

use super::cjs_wrapper_entry::{
    esbuild_commonjs_entry_thunk_params, exports_assigned_members, package_public_member_universe,
};
use crate::{ModuleMatchStrategy, PackageSource, VersionedPackageMatchReport};

/// Only de-noise packages attributed to at least this many modules. Below this,
/// attribution is plausibly genuine and is never touched — this is what keeps
/// every (small) unit-test fixture and every normally-sized package intact while
/// still trimming grossly over-attributed packages (zod 2733, glob 345, …).
const MIN_PACKAGE_MODULES_FOR_DENOISE: usize = 24;

/// Public members a module must assign to be kept as the package's genuine entry.
const MIN_GENUINE_MEMBER_ASSIGNMENTS: usize = 4;

pub(crate) fn suppress_overattributed_function_soup(
    rows: &InputRows,
    package_sources: &[PackageSource],
    report: &mut VersionedPackageMatchReport,
) {
    if report.matches.len() < MIN_PACKAGE_MODULES_FOR_DENOISE {
        return;
    }
    // count matched modules per package
    let mut module_count_by_package = BTreeMap::<&str, usize>::new();
    for package_match in &report.matches {
        *module_count_by_package
            .entry(package_match.package_name.as_str())
            .or_default() += 1;
    }
    let over_attributed = module_count_by_package
        .into_iter()
        .filter(|(_, count)| *count >= MIN_PACKAGE_MODULES_FOR_DENOISE)
        .map(|(name, _)| name.to_string())
        .collect::<BTreeSet<_>>();
    if over_attributed.is_empty() {
        return;
    }

    let members_by_package = package_public_member_universe(package_sources);
    let source_by_module = rows
        .modules
        .iter()
        .filter_map(|module| {
            rows.module_source_slice(module.id)
                .map(|slice| (module.id, slice.source))
        })
        .collect::<BTreeMap<_, _>>();

    let mut suppressed_indices = BTreeSet::<usize>::new();
    for (index, package_match) in report.matches.iter().enumerate() {
        if !over_attributed.contains(package_match.package_name.as_str()) {
            continue;
        }
        if package_match.external_importable
            || package_match.string_anchor_matches > 0
            || package_match.strategy
                != ModuleMatchStrategy::AggregateFunctionSignatureAndStringAnchors
        {
            continue;
        }
        // Keep if this module is the package's genuine entry (assigns its real
        // public members); suppress otherwise (function-soup false positive).
        let assigns_genuine_surface = members_by_package
            .get(&(
                package_match.package_name.clone(),
                package_match.package_version.clone(),
            ))
            .filter(|members| !members.is_empty())
            .and_then(|members| {
                let source = source_by_module.get(&package_match.module_id)?;
                let params = esbuild_commonjs_entry_thunk_params(source)?;
                let assigned = exports_assigned_members(source, &params);
                Some(members.intersection(&assigned).count() >= MIN_GENUINE_MEMBER_ASSIGNMENTS)
            })
            .unwrap_or(false);
        if !assigns_genuine_surface {
            suppressed_indices.insert(index);
        }
    }

    if suppressed_indices.is_empty() {
        return;
    }
    let kept = report
        .matches
        .drain(..)
        .enumerate()
        .filter_map(|(index, package_match)| {
            (!suppressed_indices.contains(&index)).then_some(package_match)
        })
        .collect::<Vec<_>>();
    report.matches = kept;
    // report.attributions holds accepted-external rows; externalized matches are
    // never suppressed here, so accepted attributions are unaffected.
}
