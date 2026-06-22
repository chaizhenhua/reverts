//! Aggregate per-unit island package anchors into per-PACKAGE externalization
//! plans.
//!
//! A scope-hoisting bundler inlines a third-party package as many CommonJS
//! module units (one per source file): a barrel/entry unit that re-exports the
//! package's public API, plus internal submodule units it pulls in. The
//! function-ownership matcher anchors the *internal* units (their bodies carry
//! real implementation), but rarely the barrel (its body is re-export glue that
//! matches weakly). Externalizing the package therefore can't work one unit at a
//! time — it needs the whole set replaced by a single `import 'pkg'` bound to
//! the barrel's exports.
//!
//! This pass groups anchored units by package and recovers the barrel
//! structurally: the barrel is the unit whose init body references the package's
//! member init functions (it calls each submodule's initializer to assemble the
//! public surface). That gives, per package, the public import specifier, the
//! entry binding to bind the namespace to, and the full set of bindings the
//! emission step will drop. It is pure analysis — no emission, no rewiring — so
//! it is safe and deterministic; the emission step consumes these plans.

use std::collections::{BTreeMap, BTreeSet};

use reverts_graph::{RecognizedCjsModule, function_referenced_names, recognize_cjs_island_modules};
use reverts_graph::RuntimePrelude;

/// One island unit binding attributed to a package — the minimal input the
/// aggregation needs (binding name + package identity), independent of the full
/// per-binding anchor record persisted to the DB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IslandUnitAttribution {
    pub binding: String,
    pub package_name: String,
    pub package_version: String,
}

/// A per-package plan for externalizing an inlined package's island units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IslandPackagePlan {
    pub package_name: String,
    pub version: String,
    /// Bare public import specifier (the package name) the inlined copy is
    /// replaced with.
    pub import_specifier: String,
    /// The barrel unit's init function — consumers call it to get the package.
    pub entry_init: String,
    /// The barrel unit's exports object — the emission step binds the imported
    /// namespace to this name so existing reads keep working.
    pub entry_exports: String,
    /// The barrel unit's guard flag, if present (dropped on externalization).
    pub entry_guard: Option<String>,
    /// Every binding the emission step removes: the barrel triple plus every
    /// anchored member unit's triple.
    pub member_bindings: BTreeSet<String>,
    /// Whether a single unambiguous barrel was recovered for this package.
    pub externalizable: bool,
    /// Why the package is not externalizable, when it is not.
    pub skip_reason: Option<String>,
}

/// Group island package anchors by package and recover each package's barrel,
/// producing one plan per package.
#[must_use]
pub fn aggregate_island_packages(
    prelude: &RuntimePrelude,
    attributions: &[IslandUnitAttribution],
) -> Vec<IslandPackagePlan> {
    let units = recognize_cjs_island_modules(&prelude.source);
    if units.is_empty() || attributions.is_empty() {
        return Vec::new();
    }

    // binding name -> (package, version), from the per-unit attributions.
    let mut package_of_binding: BTreeMap<&str, (&str, &str)> = BTreeMap::new();
    for attribution in attributions {
        package_of_binding.insert(
            attribution.binding.as_str(),
            (
                attribution.package_name.as_str(),
                attribution.package_version.as_str(),
            ),
        );
    }

    // The init-call graph: an edge i -> j means unit i's body calls unit j's
    // init. A barrel reaches each submodule it re-exports — but only
    // TRANSITIVELY (index -> api/trace -> ProxyTracerProvider -> …), so direct
    // references alone miss it; reachability over this graph is what recovers it.
    let init_to_index: BTreeMap<&str, usize> = units
        .iter()
        .enumerate()
        .map(|(index, unit)| (unit.init_fn.as_str(), index))
        .collect();
    let adjacency: Vec<Vec<usize>> = units
        .iter()
        .map(|unit| {
            unit_referenced_inits(prelude, unit, &init_to_index)
                .into_iter()
                .filter_map(|name| init_to_index.get(name.as_str()).copied())
                .collect()
        })
        .collect();

    // Group units anchored to each package.
    let mut members_by_package: BTreeMap<&str, (Vec<usize>, &str)> = BTreeMap::new();
    for (index, unit) in units.iter().enumerate() {
        let Some((package, version)) = unit_package(unit, &package_of_binding) else {
            continue;
        };
        members_by_package
            .entry(package)
            .or_insert_with(|| (Vec::new(), version))
            .0
            .push(index);
    }

    members_by_package
        .into_iter()
        .map(|(package, (member_indices, version))| {
            build_plan(package, version, &member_indices, &units, &adjacency)
        })
        .collect()
}

/// The set of unit indices transitively reachable from `start` (excluding
/// `start`) over the init-call graph, following edges through any unit.
fn reachable_from(start: usize, adjacency: &[Vec<usize>]) -> BTreeSet<usize> {
    let mut seen = BTreeSet::new();
    let mut stack = adjacency[start].clone();
    while let Some(node) = stack.pop() {
        if node == start || !seen.insert(node) {
            continue;
        }
        stack.extend(adjacency[node].iter().copied());
    }
    seen
}

/// The units DOMINATED by `barrel`: those whose every init-call predecessor is
/// itself dominated (so they are reachable only THROUGH the barrel). These are
/// the package's private units — removing them dangles nothing, because nothing
/// outside the set calls into them. Forward reachability would instead pull in
/// shared units the package merely calls (which other code also uses); those
/// are excluded here because they have a predecessor outside the set.
fn barrel_dominated_units(barrel: usize, adjacency: &[Vec<usize>]) -> BTreeSet<usize> {
    // Reverse edges: predecessors[v] = units whose init calls v's init.
    let mut predecessors: Vec<Vec<usize>> = vec![Vec::new(); adjacency.len()];
    for (caller, callees) in adjacency.iter().enumerate() {
        for &callee in callees {
            predecessors[callee].push(caller);
        }
    }
    let mut dominated: BTreeSet<usize> = BTreeSet::from([barrel]);
    loop {
        let mut grew = false;
        for (unit, preds) in predecessors.iter().enumerate() {
            if dominated.contains(&unit) || preds.is_empty() {
                continue;
            }
            // Reachable only through already-dominated units → private to barrel.
            if preds.iter().all(|pred| dominated.contains(pred)) {
                dominated.insert(unit);
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    dominated
}

/// The package a unit belongs to: the attribution of any of its three bindings.
fn unit_package<'a>(
    unit: &RecognizedCjsModule,
    package_of_binding: &BTreeMap<&str, (&'a str, &'a str)>,
) -> Option<(&'a str, &'a str)> {
    package_of_binding
        .get(unit.exports.as_str())
        .or_else(|| package_of_binding.get(unit.init_fn.as_str()))
        .or_else(|| unit.guard.as_deref().and_then(|g| package_of_binding.get(g)))
        .copied()
}

/// The member-init names a unit's body references — i.e. which other units'
/// initializers it calls. Used to find the barrel that assembles a package.
fn unit_referenced_inits(
    prelude: &RuntimePrelude,
    unit: &RecognizedCjsModule,
    init_to_index: &BTreeMap<&str, usize>,
) -> BTreeSet<String> {
    let (start, end) = (unit.body_span.0 as usize, unit.body_span.1 as usize);
    let Some(body) = prelude.source.get(start..end) else {
        return BTreeSet::new();
    };
    let mut referenced = BTreeSet::new();
    for names in function_referenced_names(body).into_values() {
        for name in names {
            if name != unit.init_fn && init_to_index.contains_key(name.as_str()) {
                referenced.insert(name);
            }
        }
    }
    referenced
}

/// Build a package plan: recover the barrel (the tightest unit that
/// transitively reaches every member submodule) and collect the bindings to
/// drop.
fn build_plan(
    package: &str,
    version: &str,
    member_indices: &[usize],
    units: &[RecognizedCjsModule],
    adjacency: &[Vec<usize>],
) -> IslandPackagePlan {
    let members: BTreeSet<usize> = member_indices.iter().copied().collect();

    // The barrel transitively reaches every member submodule (it assembles the
    // package's public surface from them). Among all such units, the barrel is
    // the TIGHTEST — the one whose reachable closure is smallest — because any
    // unit further out (e.g. an external consumer importing the package) reaches
    // the members only THROUGH the barrel, so its closure is strictly larger.
    let mut best: Option<(usize, usize, BTreeSet<usize>)> = None; // (index, closure size, closure)
    let mut ambiguous = false;
    for (index, _) in units.iter().enumerate() {
        let reachable = reachable_from(index, adjacency);
        // The unit reaches every member it does not already own.
        if !members.iter().all(|m| *m == index || reachable.contains(m)) {
            continue;
        }
        let size = reachable.len();
        match &best {
            Some((_, best_size, _)) if size == *best_size => ambiguous = true,
            Some((_, best_size, _)) if size > *best_size => {}
            _ => {
                best = Some((index, size, reachable));
                ambiguous = false;
            }
        }
    }

    match best {
        Some((entry_index, _, _)) if !ambiguous => {
            let entry = &units[entry_index];
            // The package's removal set is the barrel plus every unit it
            // DOMINATES (reachable only through it) — the package's private
            // units, including submodules that never matched (re-export glue,
            // tiny modules). Units the package merely calls but that other code
            // also uses are excluded, so the inlined copy is replaced without
            // dangling any shared dependency.
            let dominated = barrel_dominated_units(entry_index, adjacency);
            let mut member_bindings: BTreeSet<String> = BTreeSet::new();
            for &i in &dominated {
                extend_unit_bindings(&mut member_bindings, &units[i]);
            }
            IslandPackagePlan {
                package_name: package.to_string(),
                version: version.to_string(),
                import_specifier: package.to_string(),
                entry_init: entry.init_fn.clone(),
                entry_exports: entry.exports.clone(),
                entry_guard: entry.guard.clone(),
                member_bindings,
                externalizable: true,
                skip_reason: None,
            }
        }
        _ => IslandPackagePlan {
            package_name: package.to_string(),
            version: version.to_string(),
            import_specifier: package.to_string(),
            entry_init: String::new(),
            entry_exports: String::new(),
            entry_guard: None,
            member_bindings: BTreeSet::new(),
            externalizable: false,
            skip_reason: Some(if ambiguous {
                "ambiguous barrel: multiple units reach the members with equal closure size"
                    .to_string()
            } else {
                "no single unit transitively reaches all of the package's member submodules"
                    .to_string()
            }),
        },
    }
}

fn extend_unit_bindings(bindings: &mut BTreeSet<String>, unit: &RecognizedCjsModule) {
    bindings.insert(unit.exports.clone());
    bindings.insert(unit.init_fn.clone());
    if let Some(guard) = &unit.guard {
        bindings.insert(guard.clone());
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn prelude(source: &str) -> RuntimePrelude {
        RuntimePrelude {
            source_file_id: 1,
            source_file_path: "entrypoint.ts".to_string(),
            source: source.to_string(),
            bindings: BTreeMap::new(),
            snippets: BTreeMap::new(),
            namespace_exports: Vec::new(),
            entrypoint: None,
        }
    }

    fn anchor(binding: &str, package: &str) -> IslandUnitAttribution {
        IslandUnitAttribution {
            binding: binding.to_string(),
            package_name: package.to_string(),
            package_version: "1.9.0".to_string(),
        }
    }

    #[test]
    fn recovers_barrel_and_aggregates_member_bindings() {
        // Two internal submodule units (mA, mB) and a barrel (idx) that calls
        // both submodule inits to assemble its exports. Only the submodules are
        // anchored — the barrel is recovered structurally via the init-call graph.
        let source = "var eA = {};\nvar gA;\nfunction iA() { return gA || (gA = 1, eA.a = 1), eA; }\n\
                      var eB = {};\nvar gB;\nfunction iB() { return gB || (gB = 1, eB.b = 2), eB; }\n\
                      var eIdx = {};\nvar gIdx;\nfunction iIdx() { return gIdx || (gIdx = 1, eIdx.a = iA(), eIdx.b = iB()), eIdx; }\n";
        let plans = aggregate_island_packages(
            &prelude(source),
            &[anchor("eA", "pkg-x"), anchor("eB", "pkg-x")],
        );
        assert_eq!(plans.len(), 1, "{plans:?}");
        let plan = &plans[0];
        assert!(plan.externalizable, "{plan:?}");
        assert_eq!(plan.package_name, "pkg-x");
        assert_eq!(plan.import_specifier, "pkg-x");
        assert_eq!(plan.entry_init, "iIdx", "barrel recovered via init-call graph");
        assert_eq!(plan.entry_exports, "eIdx");
        // The whole set — both members and the barrel triple — is dropped.
        for binding in ["eA", "gA", "iA", "eB", "gB", "iB", "eIdx", "gIdx", "iIdx"] {
            assert!(plan.member_bindings.contains(binding), "missing {binding}: {plan:?}");
        }
    }

    #[test]
    fn no_barrel_is_not_externalizable() {
        // Two anchored submodules but nothing references both their inits.
        let source = "var eA = {};\nvar gA;\nfunction iA() { return gA || (gA = 1, eA.a = 1), eA; }\n\
                      var eB = {};\nvar gB;\nfunction iB() { return gB || (gB = 1, eB.b = 2), eB; }\n";
        let plans = aggregate_island_packages(
            &prelude(source),
            &[anchor("eA", "pkg-y"), anchor("eB", "pkg-y")],
        );
        assert_eq!(plans.len(), 1);
        assert!(!plans[0].externalizable);
        assert!(plans[0].skip_reason.is_some());
    }

    #[test]
    fn no_anchors_yields_no_plans() {
        let source = "var eA = {};\nvar gA;\nfunction iA() { return gA || (gA = 1, eA.a = 1), eA; }\n";
        assert!(aggregate_island_packages(&prelude(source), &[]).is_empty());
    }
}
