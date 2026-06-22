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

use reverts_graph::RuntimePrelude;
use reverts_graph::{RecognizedCjsModule, function_referenced_names, recognize_cjs_island_modules};
use reverts_js::{PackageIndexReexports, normalize_submodule_relpath};

/// One island unit binding attributed to a package — the minimal input the
/// aggregation needs (binding name + package identity), independent of the full
/// per-binding anchor record persisted to the DB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IslandUnitAttribution {
    pub binding: String,
    pub package_name: String,
    pub package_version: String,
    /// The submodule the binding's unit was matched to (e.g. `classes/range.js`),
    /// normalized later against a package index to synthesize a barrel. Empty when
    /// unknown (older anchor rows / non-submodule matches).
    pub export_specifier: String,
}

/// One member rebinding when a package's barrel is SYNTHESIZED from its real
/// index rather than recovered in-bundle: bind a recognized member unit's exports
/// object to the matching member(s) of the imported package namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynthesizedMemberBinding {
    /// The member unit's memoized init thunk — kept alive as `fn init(){ return local; }`.
    pub init_fn: String,
    /// The member unit's exports object binding — the rebind target.
    pub local_binding: String,
    /// How to reconstruct `local_binding` from the package namespace `ns`:
    /// a single `("", "Range")` means `local = ns.Range` (whole submodule object);
    /// multiple `(key, name)` means `local = { key: ns.name, … }` (member-pick).
    pub namespace_members: Vec<(String, String)>,
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
    /// Per-member rebindings when the barrel was SYNTHESIZED from the real index
    /// (the in-bundle barrel was tree-shaken away). Empty for the recovered-barrel
    /// case; non-empty selects the synthesized emission path.
    pub synthesized_members: Vec<SynthesizedMemberBinding>,
}

/// Group island package anchors by package and recover each package's barrel,
/// producing one plan per package.
#[must_use]
pub fn aggregate_island_packages(
    prelude: &RuntimePrelude,
    attributions: &[IslandUnitAttribution],
    index_maps: &BTreeMap<(String, String), PackageIndexReexports>,
) -> Vec<IslandPackagePlan> {
    let units = recognize_cjs_island_modules(&prelude.source);
    if units.is_empty() || attributions.is_empty() {
        return Vec::new();
    }

    // binding name -> (package, version) and binding name -> submodule relpath,
    // from the per-unit attributions.
    let mut package_of_binding: BTreeMap<&str, (&str, &str)> = BTreeMap::new();
    let mut submodule_of_binding: BTreeMap<&str, String> = BTreeMap::new();
    for attribution in attributions {
        package_of_binding.insert(
            attribution.binding.as_str(),
            (
                attribution.package_name.as_str(),
                attribution.package_version.as_str(),
            ),
        );
        if !attribution.export_specifier.is_empty() {
            // Strip the leading `<pkg>/` so the relpath matches index entries.
            let relpath = attribution
                .export_specifier
                .strip_prefix(attribution.package_name.as_str())
                .and_then(|rest| rest.strip_prefix('/'))
                .unwrap_or(attribution.export_specifier.as_str());
            submodule_of_binding.insert(
                attribution.binding.as_str(),
                normalize_submodule_relpath(relpath),
            );
        }
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
            // Per-member submodule relpath, for synthesizing a barrel from the
            // package's real index when no in-bundle barrel exists.
            let member_submodule: BTreeMap<usize, String> = member_indices
                .iter()
                .filter_map(|&index| {
                    unit_submodule(&units[index], &submodule_of_binding)
                        .map(|relpath| (index, relpath))
                })
                .collect();
            let index = index_maps.get(&(package.to_string(), version.to_string()));
            build_plan(
                package,
                version,
                &member_indices,
                &units,
                &adjacency,
                &member_submodule,
                index,
            )
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
        .or_else(|| {
            unit.guard
                .as_deref()
                .and_then(|g| package_of_binding.get(g))
        })
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

/// The submodule relpath a unit was matched to: the attribution of any of its
/// three bindings (mirrors `unit_package`).
fn unit_submodule(
    unit: &RecognizedCjsModule,
    submodule_of_binding: &BTreeMap<&str, String>,
) -> Option<String> {
    submodule_of_binding
        .get(unit.exports.as_str())
        .or_else(|| submodule_of_binding.get(unit.init_fn.as_str()))
        .or_else(|| {
            unit.guard
                .as_deref()
                .and_then(|guard| submodule_of_binding.get(guard))
        })
        .cloned()
}

/// Build a package plan: recover the barrel (the tightest unit that
/// transitively reaches every member submodule) and collect the bindings to
/// drop. When no in-bundle barrel exists but the real package index is known,
/// fall back to synthesizing one from the index.
fn build_plan(
    package: &str,
    version: &str,
    member_indices: &[usize],
    units: &[RecognizedCjsModule],
    adjacency: &[Vec<usize>],
    member_submodule: &BTreeMap<usize, String>,
    index: Option<&PackageIndexReexports>,
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
                synthesized_members: Vec::new(),
            }
        }
        _ => {
            if std::env::var_os("REVERTS_DEBUG_ISLAND_PKG").is_some() {
                eprintln!(
                    "island-pkg no-barrel {package}: members={} ambiguous={ambiguous} index={}",
                    member_indices.len(),
                    if index.is_some() { "loaded" } else { "MISSING" }
                );
            }
            // No single in-bundle barrel. If the real package index is known, the
            // package was tree-shaken (its barrel dropped) — synthesize one.
            if !ambiguous
                && let Some(index) = index
                && let Some(plan) = try_synthesize_plan(
                    package,
                    version,
                    member_indices,
                    units,
                    member_submodule,
                    index,
                )
            {
                return plan;
            }
            IslandPackagePlan {
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
                synthesized_members: Vec::new(),
            }
        }
    }
}

/// Synthesize a barrel from the real package index: every anchored member unit
/// must map (via its submodule) to at least one index re-export, giving a
/// rebinding from the package namespace. Returns `None` if any member has no
/// index entry (so the package is left inlined rather than partly externalized).
fn try_synthesize_plan(
    package: &str,
    version: &str,
    member_indices: &[usize],
    units: &[RecognizedCjsModule],
    member_submodule: &BTreeMap<usize, String>,
    index: &PackageIndexReexports,
) -> Option<IslandPackagePlan> {
    let mut synthesized_members = Vec::new();
    let mut member_bindings: BTreeSet<String> = BTreeSet::new();
    // A scope-hoisting bundler inlines each of a package's source files as ONE
    // CJS module unit, so a given submodule (e.g. `classes/range.js`) is claimed
    // by exactly one island unit. If several distinct units map to the SAME
    // submodule, the function-ownership fingerprint over-matched (a trivial
    // member body collided onto a popular submodule, dragging unrelated island
    // modules — even those of OTHER packages — in with it). Trusting that would
    // rebind every false-positive unit's exports to the same namespace member
    // (e.g. 96 unrelated bindings all set to `semver.Range`, blanking out a real
    // module's init body). The attribution is unreliable, so bail and leave the
    // package inlined rather than emit broken externalization.
    let debug = std::env::var_os("REVERTS_DEBUG_ISLAND_PKG").is_some();
    let mut seen_submodules: BTreeSet<&str> = BTreeSet::new();
    for &unit_index in member_indices {
        let unit = &units[unit_index];
        let Some(relpath) = member_submodule.get(&unit_index) else {
            if debug {
                eprintln!("island-pkg synth bail {package}: unit {unit_index} has no submodule");
            }
            return None; // unknown submodule → bail
        };
        if !seen_submodules.insert(relpath.as_str()) {
            if debug {
                let dupes: Vec<String> = member_indices
                    .iter()
                    .filter(|&&i| member_submodule.get(&i).map(String::as_str) == Some(relpath.as_str()))
                    .map(|&i| {
                        format!(
                            "{}(init={},binds={})",
                            i,
                            units[i].init_fn,
                            unit_binding_count(&units[i])
                        )
                    })
                    .collect();
                eprintln!(
                    "island-pkg synth bail {package}: duplicate submodule '{relpath}' units=[{}]",
                    dupes.join(", ")
                );
            }
            return None; // duplicate submodule → fingerprint over-match → not safe
        }
        let reexports = index.for_submodule(relpath);
        if reexports.is_empty() {
            if debug {
                eprintln!(
                    "island-pkg synth bail {package}: submodule '{relpath}' not re-exported by index"
                );
            }
            return None; // member not re-exported by the index → cannot externalize cleanly
        }
        let namespace_members: Vec<(String, String)> = reexports
            .iter()
            .map(|reexport| {
                (
                    reexport.member.clone().unwrap_or_default(),
                    reexport.export_name.clone(),
                )
            })
            .collect();
        synthesized_members.push(SynthesizedMemberBinding {
            init_fn: unit.init_fn.clone(),
            local_binding: unit.exports.clone(),
            namespace_members,
        });
        extend_unit_bindings(&mut member_bindings, unit);
    }
    if synthesized_members.is_empty() {
        return None;
    }
    Some(IslandPackagePlan {
        package_name: package.to_string(),
        version: version.to_string(),
        import_specifier: package.to_string(),
        entry_init: String::new(),
        entry_exports: String::new(),
        entry_guard: None,
        member_bindings,
        externalizable: true,
        skip_reason: None,
        synthesized_members,
    })
}

fn unit_binding_count(unit: &RecognizedCjsModule) -> usize {
    let mut set = BTreeSet::new();
    extend_unit_bindings(&mut set, unit);
    set.len()
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
            export_specifier: String::new(),
        }
    }

    fn anchor_sub(binding: &str, package: &str, specifier: &str) -> IslandUnitAttribution {
        IslandUnitAttribution {
            export_specifier: specifier.to_string(),
            ..anchor(binding, package)
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
            &BTreeMap::new(),
        );
        assert_eq!(plans.len(), 1, "{plans:?}");
        let plan = &plans[0];
        assert!(plan.externalizable, "{plan:?}");
        assert_eq!(plan.package_name, "pkg-x");
        assert_eq!(plan.import_specifier, "pkg-x");
        assert_eq!(
            plan.entry_init, "iIdx",
            "barrel recovered via init-call graph"
        );
        assert_eq!(plan.entry_exports, "eIdx");
        // The whole set — both members and the barrel triple — is dropped.
        for binding in ["eA", "gA", "iA", "eB", "gB", "iB", "eIdx", "gIdx", "iIdx"] {
            assert!(
                plan.member_bindings.contains(binding),
                "missing {binding}: {plan:?}"
            );
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
            &BTreeMap::new(),
        );
        assert_eq!(plans.len(), 1);
        assert!(!plans[0].externalizable);
        assert!(plans[0].skip_reason.is_some());
    }

    #[test]
    fn synthesizes_barrel_from_index_when_no_in_bundle_barrel() {
        // Two anchored submodules, no in-bundle barrel — but the real index maps
        // each submodule to a namespace member, so synthesis externalizes anyway.
        let source = "var eA = {};\nvar gA;\nfunction iA() { return gA || (gA = 1, eA.a = 1), eA; }\n\
                      var eB = {};\nvar gB;\nfunction iB() { return gB || (gB = 1, eB.b = 2), eB; }\n";
        let mut index_maps = BTreeMap::new();
        index_maps.insert(
            ("pkg-z".to_string(), "1.9.0".to_string()),
            reverts_js::parse_index_reexports(
                "module.exports = { A: require('./classes/a'), B: require('./classes/b') };",
            ),
        );
        let plans = aggregate_island_packages(
            &prelude(source),
            &[
                anchor_sub("eA", "pkg-z", "pkg-z/classes/a.js"),
                anchor_sub("eB", "pkg-z", "pkg-z/classes/b.js"),
            ],
            &index_maps,
        );
        assert_eq!(plans.len(), 1, "{plans:?}");
        let plan = &plans[0];
        assert!(plan.externalizable, "{plan:?}");
        assert_eq!(plan.synthesized_members.len(), 2, "{plan:?}");
        let member_a = plan
            .synthesized_members
            .iter()
            .find(|member| member.local_binding == "eA")
            .expect("eA member");
        assert_eq!(
            member_a.namespace_members,
            vec![(String::new(), "A".to_string())]
        );
        assert_eq!(member_a.init_fn, "iA");
    }

    #[test]
    fn over_matched_duplicate_submodule_is_not_synthesized() {
        // Two distinct island units (eA, eB) both anchored to the SAME submodule
        // (`classes/a.js`) — the signature of a function-ownership fingerprint
        // over-match (a trivial body collided onto one popular submodule, sweeping
        // in an unrelated module). Synthesis must bail and leave the package
        // inlined rather than rebind both exports to the same `ns.A` (which would
        // blank out the second module's real init body — the semver/`Range`
        // regression that broke RxJS's Scheduler).
        let source = "var eA = {};\nvar gA;\nfunction iA() { return gA || (gA = 1, eA.a = 1), eA; }\n\
                      var eB = {};\nvar gB;\nfunction iB() { return gB || (gB = 1, eB.b = 2), eB; }\n";
        let mut index_maps = BTreeMap::new();
        index_maps.insert(
            ("pkg-z".to_string(), "1.9.0".to_string()),
            reverts_js::parse_index_reexports(
                "module.exports = { A: require('./classes/a'), B: require('./classes/b') };",
            ),
        );
        let plans = aggregate_island_packages(
            &prelude(source),
            &[
                anchor_sub("eA", "pkg-z", "pkg-z/classes/a.js"),
                anchor_sub("eB", "pkg-z", "pkg-z/classes/a.js"),
            ],
            &index_maps,
        );
        assert_eq!(plans.len(), 1, "{plans:?}");
        assert!(
            !plans[0].externalizable,
            "duplicate submodule must not externalize: {:?}",
            plans[0]
        );
    }

    #[test]
    fn no_anchors_yields_no_plans() {
        let source =
            "var eA = {};\nvar gA;\nfunction iA() { return gA || (gA = 1, eA.a = 1), eA; }\n";
        assert!(aggregate_island_packages(&prelude(source), &[], &BTreeMap::new()).is_empty());
    }
}
