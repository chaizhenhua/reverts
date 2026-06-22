//! Behavior-preserving extraction of one island cluster into its own file.
//!
//! Splitting the eager entry island is only safe for statements that carry no
//! evaluation-order meaning. A scope-hoisted island runs top-to-bottom and
//! contains side-effecting top-level statements (e.g. a library's global
//! registry init) and order-dependent initializers; moving those across a module
//! boundary changes when they run. Only hoisted `function` *declarations* are
//! safe to relocate: they are defined by hoisting and are inert until called, so
//! moving one to an imported module and importing it back does not change
//! observable behavior — provided the binding is never reassigned (an ES import
//! is read-only) and the function does not mutate shared module state (which
//! would fork once relocated; see `top_level_functions_writing_module_state`).
//!
//! Classes are NOT moved even though they are declarations: a `class` evaluates
//! its `extends` base and any static field/block at definition time, and esbuild
//! initializes an imported cluster module *before* the entry that imports it — so
//! a relocated `class X extends na {}` runs `extends na` before the entry defines
//! the eager binding `na`, crashing. Equivalence on the real bundle was verified
//! by comparing the recovered split's module-init interaction trace against the
//! unsplit recovery: identical multiset, no load error.
//!
//! This module performs only the source-level extraction: which statements move,
//! the moved file's body, and the remainder. Import/export wiring between the
//! remainder and the extracted file is layered on by `cli_entrypoint`.

use std::collections::{BTreeMap, BTreeSet};

use reverts_ir::BindingName;
use reverts_js::{
    ParseGoal, TopLevelStatementKind, collect_identifier_read_facts,
    collect_top_level_statement_facts,
};
use reverts_model::IslandPackageExternalization;

use crate::statements::{named_export_statement, named_import_statement};

/// The result of lifting a cluster's hoistable declarations out of the island.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClusterExtraction {
    /// Bindings whose declarations were moved into the cluster file.
    pub(crate) moved_bindings: BTreeSet<BindingName>,
    /// Source of the extracted declarations, in original order.
    pub(crate) cluster_source: String,
    /// The island source with the extracted declarations removed.
    pub(crate) remaining_source: String,
}

/// Extract the hoistable (`function`/`class`) declarations whose bindings belong
/// to `cluster_bindings` out of `island_source`.
///
/// A declaration is moved only when it is a hoisted function declaration, every
/// binding it introduces is in `cluster_bindings`, and none of those bindings is
/// in `written_bindings` (a reassigned binding cannot become a read-only import).
/// Every other statement — side-effecting expressions, variable initializers,
/// class declarations, imports/exports, setters, lazy thunks — stays in
/// `remaining_source` in its original position, preserving evaluation order and
/// side effects.
///
/// Classes are deliberately NOT moved: unlike a function declaration (hoisted,
/// inert until called), a `class` declaration evaluates its `extends` base and
/// any static field/block at definition time. esbuild initializes an imported
/// cluster module *before* the entry that imports it, so a moved
/// `class X extends na {}` would run its `extends na` before the entry defines
/// the eager binding `na` — a `Class extends undefined` crash. Relocating
/// classes safely needs eager-dependency analysis; functions need none.
///
/// Returns `None` when nothing is extractable (so callers leave the island
/// untouched) or when the source cannot be parsed.
pub(crate) fn extract_hoistable_cluster(
    island_source: &str,
    cluster_bindings: &BTreeSet<BindingName>,
    written_bindings: &BTreeSet<BindingName>,
) -> Option<ClusterExtraction> {
    let facts =
        collect_top_level_statement_facts(island_source, None, ParseGoal::TypeScript).ok()?;

    // Byte ranges of the statements to lift, in source order.
    let mut extracted: Vec<(usize, usize)> = Vec::new();
    let mut moved_bindings: BTreeSet<BindingName> = BTreeSet::new();
    for fact in &facts {
        if !matches!(fact.kind, TopLevelStatementKind::Function) {
            continue;
        }
        if fact.bindings.is_empty() {
            continue;
        }
        let bindings: Vec<BindingName> = fact
            .bindings
            .iter()
            .map(|name| BindingName::new(name.as_str()))
            .collect();
        let all_in_cluster = bindings
            .iter()
            .all(|binding| cluster_bindings.contains(binding));
        let any_written = bindings
            .iter()
            .any(|binding| written_bindings.contains(binding));
        if !all_in_cluster || any_written {
            continue;
        }
        extracted.push((fact.byte_start as usize, fact.byte_end as usize));
        moved_bindings.extend(bindings);
    }

    if extracted.is_empty() {
        return None;
    }

    let cluster_source = extracted
        .iter()
        .map(|&(start, end)| island_source.get(start..end).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    let remaining_source = remove_ranges(island_source, &extracted);

    Some(ClusterExtraction {
        moved_bindings,
        cluster_source,
        remaining_source,
    })
}

/// Return `source` with every byte range in `ranges` removed. Ranges are assumed
/// non-overlapping (distinct top-level statements); they are sorted here so the
/// kept gaps stay in order.
fn remove_ranges(source: &str, ranges: &[(usize, usize)]) -> String {
    let mut sorted = ranges.to_vec();
    sorted.sort_unstable();
    let mut remaining = String::with_capacity(source.len());
    let mut cursor = 0;
    for (start, end) in sorted {
        if start > cursor {
            remaining.push_str(source.get(cursor..start).unwrap_or_default());
        }
        cursor = cursor.max(end);
    }
    if cursor < source.len() {
        remaining.push_str(source.get(cursor..).unwrap_or_default());
    }
    remaining
}

/// One cluster's extracted declarations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClusterGroup {
    pub(crate) cluster_id: usize,
    pub(crate) moved_bindings: BTreeSet<BindingName>,
    pub(crate) cluster_source: String,
}

/// The result of partitioning the whole island in a single parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IslandPartition {
    pub(crate) remaining_source: String,
    pub(crate) clusters: Vec<ClusterGroup>,
}

/// Partition the island in ONE parse: assign each hoistable (`function`/`class`)
/// declaration whose bindings share a cluster to that cluster, leaving every
/// other statement — and any declaration that is reassigned, spans multiple
/// clusters, or has an unclustered binding — in `remaining_source` in place.
///
/// Re-parsing per cluster would be O(clusters × source); the island has
/// thousands of clusters over a multi-megabyte source, so a single parse is
/// required. Returns `None` if the source does not parse or nothing is movable.
pub(crate) fn partition_island_into_clusters(
    island_source: &str,
    binding_to_cluster: &BTreeMap<BindingName, usize>,
    written_bindings: &BTreeSet<BindingName>,
    force_move_variables: &BTreeSet<BindingName>,
    movable_classes: &BTreeSet<BindingName>,
) -> Option<IslandPartition> {
    let facts =
        collect_top_level_statement_facts(island_source, None, ParseGoal::TypeScript).ok()?;

    // cluster_id -> (statement byte ranges, moved bindings)
    type Accumulator = (Vec<(usize, usize)>, BTreeSet<BindingName>);
    let mut groups: BTreeMap<usize, Accumulator> = BTreeMap::new();
    let mut extracted_ranges: Vec<(usize, usize)> = Vec::new();
    for fact in &facts {
        if fact.bindings.is_empty() {
            continue;
        }
        let names: Vec<BindingName> = fact
            .bindings
            .iter()
            .map(|name| BindingName::new(name.as_str()))
            .collect();
        // A hoisted `function` declaration is always eval-order-safe to relocate
        // across the cyclic entry<->cluster import boundary (it is inert until
        // called). A `var` declaration moves ONLY when every binding it declares
        // is a force-move variable — a recognized CJS module's exports/guard,
        // co-moved with its init function so the writer and the shared state it
        // writes relocate together and nothing is forked. A `class` moves ONLY
        // when the caller proved it eval-order-safe (its definition-time
        // references — `extends`, decorators, static initializers — touch no
        // eager binding that initializes after the cluster loads).
        let movable = match fact.kind {
            TopLevelStatementKind::Function => !names
                .iter()
                .any(|binding| written_bindings.contains(binding)),
            TopLevelStatementKind::Variable => names
                .iter()
                .all(|binding| force_move_variables.contains(binding)),
            TopLevelStatementKind::Class => {
                names
                    .iter()
                    .all(|binding| movable_classes.contains(binding))
                    && !names
                        .iter()
                        .any(|binding| written_bindings.contains(binding))
            }
            _ => false,
        };
        if !movable {
            continue;
        }
        // Every binding must map to the SAME cluster; otherwise the statement
        // straddles a boundary and stays whole in the island.
        let mut cluster = None;
        let mut same_cluster = true;
        for binding in &names {
            match binding_to_cluster.get(binding) {
                Some(&id) => match cluster {
                    None => cluster = Some(id),
                    Some(existing) if existing != id => {
                        same_cluster = false;
                        break;
                    }
                    Some(_) => {}
                },
                None => {
                    same_cluster = false;
                    break;
                }
            }
        }
        let Some(cluster_id) = cluster.filter(|_| same_cluster) else {
            continue;
        };
        let range = (fact.byte_start as usize, fact.byte_end as usize);
        let entry = groups.entry(cluster_id).or_default();
        entry.0.push(range);
        entry.1.extend(names);
        extracted_ranges.push(range);
    }

    if extracted_ranges.is_empty() {
        return None;
    }

    let clusters = groups
        .into_iter()
        .map(|(cluster_id, (ranges, moved_bindings))| {
            let cluster_source = ranges
                .iter()
                .map(|&(start, end)| island_source.get(start..end).unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n");
            ClusterGroup {
                cluster_id,
                moved_bindings,
                cluster_source,
            }
        })
        .collect();
    let remaining_source = remove_ranges(island_source, &extracted_ranges);

    Some(IslandPartition {
        remaining_source,
        clusters,
    })
}

/// Estimate how many lines `source` occupies AFTER the emitter pretty-prints it.
/// Cluster bodies arrive minified — many statements per physical line — so a raw
/// newline count under-measures the emitted file by an order of magnitude. The
/// formatter emits roughly one statement terminator or brace per line, so those
/// tokens predict the emitted line count far better. (Tokens inside string/regex
/// literals are counted too; that only over-estimates, which errs toward smaller
/// files — the safe direction for a budget.)
fn estimated_emitted_lines(source: &str) -> usize {
    // `;{}` track statement/block lines; `,` and brackets track the one-element-
    // per-line expansion of large object/array data literals, which otherwise read
    // as a handful of physical lines but format into thousands.
    source
        .bytes()
        .filter(|&byte| matches!(byte, b';' | b'{' | b'}' | b',' | b'[' | b']'))
        .count()
}

/// Split any cluster whose body exceeds `max_body_lines` into size-bounded
/// sub-clusters, so no emitted file blows past the per-file line budget. Every
/// declaration a cluster holds is eval-order-independent (hoisted functions,
/// inert function-expression bindings, eval-safe classes — never an interdependent
/// init triple, which always forms its own dedicated cluster), so distributing
/// them across files only changes which file each lives in; cross-references
/// between the pieces resolve to direct imports through the normal binding-owner
/// wiring. A single declaration larger than the budget is left whole (it cannot
/// be split). Greedy bin-packing in source order keeps the result deterministic.
///
/// Re-parses each oversized cluster's already-extracted body (cheap: bodies are
/// a fraction of the island) to recover statement boundaries. The first sub-bin
/// keeps the original `cluster_id`; the rest take ids from `next_cluster_id`.
pub(crate) fn split_oversized_clusters(
    clusters: Vec<ClusterGroup>,
    max_body_lines: usize,
    next_cluster_id: &mut usize,
) -> Vec<ClusterGroup> {
    // The estimator predicts emitted lines from statement-terminator/brace tokens;
    // formatted code also has brace-less continuation lines it cannot see, so it
    // can under-count. Pack to 80% of the budget so even a ~25% under-count stays
    // within it.
    let target = max_body_lines * 4 / 5;
    let mut out = Vec::new();
    for group in clusters {
        split_cluster(group, target, 0, next_cluster_id, &mut out);
    }
    out
}

/// Guards the recursion: even a pathological reference graph collapses to size
/// bin-packing after this many levels of community subdivision.
const MAX_SPLIT_DEPTH: usize = 8;

/// Bring `group` within the budget. PRIMARY mechanism: recover cohesive
/// sub-modules by community detection over the declarations' own reference graph
/// (recursively, so a still-oversized sub-module subdivides again). FALLBACK
/// (size bin-packing in source order) only when the declarations form a single
/// indivisible community — a flat blob with no internal module structure — or the
/// recursion budget is spent. A lone declaration larger than the budget is left
/// whole (it cannot be divided). Everything a size-cap cluster holds is
/// eval-order-independent, so regrouping across files is behavior-preserving.
fn split_cluster(
    group: ClusterGroup,
    target: usize,
    depth: usize,
    next_cluster_id: &mut usize,
    out: &mut Vec<ClusterGroup>,
) {
    if estimated_emitted_lines(&group.cluster_source) <= target {
        out.push(group);
        return;
    }
    let Ok(facts) =
        collect_top_level_statement_facts(&group.cluster_source, None, ParseGoal::TypeScript)
    else {
        out.push(group); // cannot re-parse → keep whole rather than risk a bad split
        return;
    };
    let decls: Vec<(&str, Vec<BindingName>)> = facts
        .iter()
        .filter(|fact| !fact.bindings.is_empty())
        .map(|fact| {
            let slice = group
                .cluster_source
                .get(fact.byte_start as usize..fact.byte_end as usize)
                .unwrap_or_default();
            let bindings = fact
                .bindings
                .iter()
                .map(|name| BindingName::new(name.as_str()))
                .collect();
            (slice, bindings)
        })
        .collect();
    if decls.len() <= 1 {
        out.push(group); // a single declaration cannot be divided
        return;
    }

    let subgroups = if depth < MAX_SPLIT_DEPTH {
        semantic_subgroups(&decls)
    } else {
        Vec::new()
    };
    let bins = if subgroups.len() > 1 {
        // Pack whole communities into budget-sized bins: combine small cohesive
        // communities into one file rather than emitting thousands of singletons,
        // while a community that alone exceeds the budget becomes its own bin and
        // recurses (subdividing or, ultimately, bin-packing).
        pack_groups_by_size(subgroups, target)
    } else {
        bin_pack_by_size(&decls, target)
    };

    for (index, (slices, bindings)) in bins.into_iter().enumerate() {
        let cluster_id = if index == 0 {
            group.cluster_id
        } else {
            let id = *next_cluster_id;
            *next_cluster_id += 1;
            id
        };
        let subgroup = ClusterGroup {
            cluster_id,
            moved_bindings: bindings,
            cluster_source: slices.join("\n"),
        };
        split_cluster(subgroup, target, depth + 1, next_cluster_id, out);
    }
}

/// Partition declarations into cohesive sub-modules by one level of community
/// detection over the reference graph they form among themselves. Returns the
/// groups (slices + bindings) in community order; a result of length ≤ 1 means
/// the declarations are one indivisible community.
fn semantic_subgroups<'a>(
    decls: &[(&'a str, Vec<BindingName>)],
) -> Vec<(Vec<&'a str>, BTreeSet<BindingName>)> {
    let own: BTreeSet<BindingName> = decls
        .iter()
        .flat_map(|(_, bindings)| bindings.iter().cloned())
        .collect();
    let mut references: BTreeMap<BindingName, BTreeSet<BindingName>> = BTreeMap::new();
    for (slice, bindings) in decls {
        let refs: BTreeSet<BindingName> =
            crate::runtime_source_scan::value_identifiers_in_source(slice)
                .into_iter()
                .map(BindingName::new)
                .filter(|reference| own.contains(reference))
                .collect();
        for binding in bindings {
            let mut binding_refs = refs.clone();
            binding_refs.remove(binding);
            references
                .entry(binding.clone())
                .or_default()
                .extend(binding_refs);
        }
    }
    let community = crate::island_clustering::cluster_bindings_by_references(&references);

    let mut by_community: BTreeMap<usize, (Vec<&str>, BTreeSet<BindingName>)> = BTreeMap::new();
    for (slice, bindings) in decls {
        let id = bindings
            .first()
            .and_then(|binding| community.get(binding))
            .copied()
            .unwrap_or(usize::MAX);
        let entry = by_community.entry(id).or_default();
        entry.0.push(slice);
        entry.1.extend(bindings.iter().cloned());
    }
    by_community.into_values().collect()
}

/// Greedy-pack whole groups (cohesive communities) into budget-sized bins,
/// keeping each community intact. Small sibling communities combine into one
/// file; a community that alone exceeds the budget becomes its own bin (the
/// caller then recurses to subdivide it). Avoids the thousand-singleton-file
/// explosion of emitting one file per community.
fn pack_groups_by_size(
    groups: Vec<(Vec<&str>, BTreeSet<BindingName>)>,
    target: usize,
) -> Vec<(Vec<&str>, BTreeSet<BindingName>)> {
    let mut bins: Vec<(Vec<&str>, BTreeSet<BindingName>)> = Vec::new();
    let mut slices: Vec<&str> = Vec::new();
    let mut bindings: BTreeSet<BindingName> = BTreeSet::new();
    let mut lines = 0;
    for (group_slices, group_bindings) in groups {
        let group_lines: usize = group_slices
            .iter()
            .map(|slice| estimated_emitted_lines(slice).max(1))
            .sum();
        if !slices.is_empty() && lines + group_lines > target {
            bins.push((std::mem::take(&mut slices), std::mem::take(&mut bindings)));
            lines = 0;
        }
        slices.extend(group_slices);
        bindings.extend(group_bindings);
        lines += group_lines;
    }
    if !slices.is_empty() {
        bins.push((slices, bindings));
    }
    bins
}

/// Greedy size bin-packing of declarations in source order — the fallback for a
/// flat blob with no recoverable internal module structure.
fn bin_pack_by_size<'a>(
    decls: &[(&'a str, Vec<BindingName>)],
    target: usize,
) -> Vec<(Vec<&'a str>, BTreeSet<BindingName>)> {
    let mut bins: Vec<(Vec<&str>, BTreeSet<BindingName>)> = Vec::new();
    let mut slices: Vec<&str> = Vec::new();
    let mut bindings: BTreeSet<BindingName> = BTreeSet::new();
    let mut lines = 0;
    for (slice, decl_bindings) in decls {
        let slice_lines = estimated_emitted_lines(slice).max(1);
        if !slices.is_empty() && lines + slice_lines > target {
            bins.push((std::mem::take(&mut slices), std::mem::take(&mut bindings)));
            lines = 0;
        }
        slices.push(slice);
        bindings.extend(decl_bindings.iter().cloned());
        lines += slice_lines;
    }
    if !slices.is_empty() {
        bins.push((slices, bindings));
    }
    bins
}

/// Assemble the source of an extracted cluster file: an import per source
/// specifier for the external bindings the moved declarations reference, then
/// the declarations, then a single re-export of every moved binding.
///
/// `imports` maps an emit-relative module specifier to the bindings to import
/// from it — every name the cluster references that is not declared in the
/// cluster itself (resolved by the caller from binding ownership). Imports are
/// emitted in specifier order for determinism.
pub(crate) fn assemble_cluster_file(
    cluster_source: &str,
    moved_bindings: &BTreeSet<BindingName>,
    imports: &BTreeMap<String, BTreeSet<BindingName>>,
) -> String {
    let mut out = String::new();
    for (specifier, names) in imports {
        if names.is_empty() {
            continue;
        }
        out.push_str(named_import_statement(names.iter(), specifier).as_str());
        out.push('\n');
    }
    out.push_str(cluster_source);
    out.push('\n');
    out.push_str(named_export_statement(moved_bindings.iter()).as_str());
    out.push('\n');
    out
}

/// The statement the island entry uses to import a cluster's moved bindings back
/// from the cluster file at `specifier`.
pub(crate) fn entry_import_for_cluster(
    moved_bindings: &BTreeSet<BindingName>,
    specifier: &str,
) -> String {
    named_import_statement(moved_bindings.iter(), specifier)
}

/// The island after inlined packages were replaced with bare imports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IslandExternalization {
    /// Island source with every externalized package's unit declarations removed
    /// and a barrel-init shim appended for each.
    pub(crate) source: String,
    /// Bare `import * as … from '…'` lines to emit at the top of the island.
    pub(crate) imports: Vec<String>,
    /// Every binding removed from the island (left the naming denominator).
    pub(crate) externalized_bindings: BTreeSet<BindingName>,
    /// The barrel exports bindings now provided by an `import * as …` — these
    /// remain bound in the island (as import bindings), unlike the removed
    /// internal members.
    pub(crate) entry_bindings: BTreeSet<BindingName>,
}

/// Replace each inlined package's CommonJS units with a bare import.
///
/// For a package whose barrel exports object is `E`, init is `I`, and member
/// bindings are `M`: delete every top-level statement that declares only
/// bindings in `M`, emit `import * as E from '<specifier>'`, and append a shim
/// `function I() { return E; }` so both existing `I()` callers and direct `E`
/// reads keep working against the real package.
///
/// A package is externalized only when it is SAFE: every internal member (a
/// member that is neither the barrel exports nor the barrel init) must be
/// referenced nowhere in the island except the statements being removed — i.e.
/// the inlined copy is self-contained behind its barrel. A package failing that
/// gate is left inlined (skipped) rather than risk a dangling reference.
///
/// Returns `None` if nothing was externalized.
pub(crate) fn externalize_island_packages(
    island_source: &str,
    packages: &[IslandPackageExternalization],
) -> Option<IslandExternalization> {
    if packages.is_empty() {
        return None;
    }
    let facts =
        collect_top_level_statement_facts(island_source, None, ParseGoal::TypeScript).ok()?;

    // True identifier READS across the island (name + span), excluding property
    // keys, member accesses, and string contents — so the safety gate cannot be
    // tripped by an object key that merely shares a member's minified name.
    let read_facts =
        collect_identifier_read_facts(island_source, None, ParseGoal::TypeScript).ok()?;
    let mut reads_by_name: BTreeMap<&str, Vec<(u32, u32)>> = BTreeMap::new();
    for fact in &read_facts {
        reads_by_name
            .entry(fact.name.as_str())
            .or_default()
            .push((fact.byte_start, fact.byte_end));
    }

    // Map each binding to the statement ranges that declare ONLY package members,
    // so a straddling statement (declares a member + a non-member) is never cut.
    let mut imports = Vec::new();
    let mut removed_ranges: Vec<(usize, usize)> = Vec::new();
    let mut externalized_bindings: BTreeSet<BindingName> = BTreeSet::new();
    let mut entry_bindings: BTreeSet<BindingName> = BTreeSet::new();
    let mut shims = String::new();

    for package in packages {
        // Candidate removals: statements whose every binding is a member.
        let mut package_ranges: Vec<(usize, usize)> = Vec::new();
        let mut covered: BTreeSet<BindingName> = BTreeSet::new();
        for fact in &facts {
            if fact.bindings.is_empty() {
                continue;
            }
            let names: Vec<BindingName> = fact
                .bindings
                .iter()
                .map(|n| BindingName::new(n.as_str()))
                .collect();
            if names
                .iter()
                .all(|binding| package.member_bindings.contains(binding))
            {
                package_ranges.push((fact.byte_start as usize, fact.byte_end as usize));
                covered.extend(names);
            }
        }

        // Safety gate: every INTERNAL member (not the barrel surface) must be
        // declared by a removable statement and referenced only within them.
        let internal: BTreeSet<&BindingName> = package
            .member_bindings
            .iter()
            .filter(|binding| **binding != package.entry_exports && **binding != package.entry_init)
            .collect();
        let all_covered = internal.iter().all(|binding| covered.contains(*binding));
        // An internal member is referenced outside the package iff it has an
        // identifier READ whose span lies outside every removed statement range.
        let referenced_outside = internal.iter().any(|binding| {
            reads_by_name.get(binding.as_str()).is_some_and(|spans| {
                spans
                    .iter()
                    .any(|span| !span_within_any(*span, &package_ranges))
            })
        });
        if !all_covered || referenced_outside || package_ranges.is_empty() {
            continue; // leave this package inlined — not provably safe
        }

        // Bind the barrel exports to the real package via a CJS-interop import:
        // the inlined barrel produced the package's `module.exports` object, so
        // `<exports>` must be that object — `ns.default ?? ns` yields it whether
        // the package is CommonJS (default = module.exports) or ESM (namespace),
        // and keeps named access working through a bundler's ESM-namespace wrap.
        // The namespace import + const binding go at the top (imports hoist; the
        // const initializes before the island body reads `<exports>`); the barrel
        // init shim is a hoisted function appended at the end.
        let namespace_alias = format!("{}__ext", package.entry_exports.as_str());
        imports.push(format!(
            "import * as {namespace_alias} from '{}';",
            package.import_specifier
        ));
        imports.push(format!(
            "const {} = {namespace_alias}.default ?? {namespace_alias};",
            package.entry_exports.as_str()
        ));
        shims.push_str(&format!(
            "function {}() {{ return {}; }}\n",
            package.entry_init.as_str(),
            package.entry_exports.as_str()
        ));
        removed_ranges.extend(package_ranges);
        externalized_bindings.extend(covered);
        entry_bindings.insert(package.entry_exports.clone());
    }

    if removed_ranges.is_empty() {
        return None;
    }

    let mut source = remove_ranges(island_source, &removed_ranges);
    if !shims.is_empty() {
        source.push('\n');
        source.push_str(&shims);
    }
    Some(IslandExternalization {
        source,
        imports,
        externalized_bindings,
        entry_bindings,
    })
}

/// Whether the read span `(start, end)` is contained in any removed statement
/// range — i.e. the reference is internal to the package being externalized.
fn span_within_any(span: (u32, u32), ranges: &[(usize, usize)]) -> bool {
    let (start, end) = (span.0 as usize, span.1 as usize);
    ranges
        .iter()
        .any(|&(range_start, range_end)| range_start <= start && end <= range_end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bindings(names: &[&str]) -> BTreeSet<BindingName> {
        names.iter().map(|name| BindingName::new(*name)).collect()
    }

    fn externalization(
        specifier: &str,
        init: &str,
        exports: &str,
        members: &[&str],
    ) -> IslandPackageExternalization {
        IslandPackageExternalization {
            import_specifier: specifier.to_string(),
            version: "1.0.0".to_string(),
            entry_init: BindingName::new(init),
            entry_exports: BindingName::new(exports),
            member_bindings: bindings(members),
        }
    }

    #[test]
    fn externalizes_a_self_contained_inlined_package() {
        // An inlined package: internal submodule (eA/gA/iA), barrel (eIdx/gIdx/iIdx).
        // A consumer calls the barrel init. The whole package is replaced by an
        // import; the barrel init becomes a shim; the consumer keeps working.
        let island = "var eA = {};\nvar gA;\nfunction iA() { return gA || (gA = 1, eA.a = 1), eA; }\n\
                      var eIdx = {};\nvar gIdx;\nfunction iIdx() { return gIdx || (gIdx = 1, eIdx.a = iA()), eIdx; }\n\
                      var consumer = iIdx();\n";
        let result = externalize_island_packages(
            island,
            &[externalization(
                "pkg-x",
                "iIdx",
                "eIdx",
                &["eA", "gA", "iA", "eIdx", "gIdx", "iIdx"],
            )],
        )
        .expect("should externalize");

        assert_eq!(
            result.imports,
            vec![
                "import * as eIdx__ext from 'pkg-x';".to_string(),
                "const eIdx = eIdx__ext.default ?? eIdx__ext;".to_string(),
            ]
        );
        // All unit declarations removed; the consumer survives.
        assert!(
            !result.source.contains("function iA()"),
            "{}",
            result.source
        );
        assert!(!result.source.contains("var eA = {}"), "{}", result.source);
        assert!(
            result.source.contains("var consumer = iIdx();"),
            "{}",
            result.source
        );
        // Barrel init shim returns the imported namespace.
        assert!(
            result.source.contains("function iIdx() { return eIdx; }"),
            "{}",
            result.source
        );
        assert!(
            result
                .externalized_bindings
                .contains(&BindingName::new("iA"))
        );
    }

    #[test]
    fn skips_package_whose_internal_member_is_referenced_outside() {
        // `iA` (an internal submodule init) is also called directly by external
        // code — removing it would dangle, so the package is left inlined.
        let island = "var eA = {};\nvar gA;\nfunction iA() { return gA || (gA = 1, eA.a = 1), eA; }\n\
                      var eIdx = {};\nvar gIdx;\nfunction iIdx() { return gIdx || (gIdx = 1, eIdx.a = iA()), eIdx; }\n\
                      var direct = iA();\n";
        let result = externalize_island_packages(
            island,
            &[externalization(
                "pkg-x",
                "iIdx",
                "eIdx",
                &["eA", "gA", "iA", "eIdx", "gIdx", "iIdx"],
            )],
        );
        assert!(
            result.is_none(),
            "internal member referenced outside -> not safe"
        );
    }

    #[test]
    fn empty_package_list_externalizes_nothing() {
        assert!(externalize_island_packages("var a = 1;", &[]).is_none());
    }

    /// Golden snapshot of the end-to-end externalization emission for a realistic
    /// inlined package: two internal submodule units (`diag`, `ctx`) plus a
    /// barrel (`api`) that assembles them, and a consumer that calls the barrel.
    /// The golden pins exactly what ships: the CJS-interop import pair, the kept
    /// consumer, and the barrel-init shim — with every inlined unit removed.
    #[test]
    fn golden_externalized_inlined_package_emission() {
        let island = "\
var diagExports = {};
var diagGuard;
function diagInit() { return diagGuard || (diagGuard = 1, diagExports.createLogger = function() { return 1; }), diagExports; }
var ctxExports = {};
var ctxGuard;
function ctxInit() { return ctxGuard || (ctxGuard = 1, ctxExports.active = function() { return 2; }), ctxExports; }
var apiExports = {};
var apiGuard;
function apiInit() { return apiGuard || (apiGuard = 1, apiExports.diag = diagInit(), apiExports.context = ctxInit()), apiExports; }
var theApi = apiInit();
";
        let result = externalize_island_packages(
            island,
            &[externalization(
                "@scope/api",
                "apiInit",
                "apiExports",
                &[
                    "diagExports",
                    "diagGuard",
                    "diagInit",
                    "ctxExports",
                    "ctxGuard",
                    "ctxInit",
                    "apiExports",
                    "apiGuard",
                    "apiInit",
                ],
            )],
        )
        .expect("self-contained package externalizes");

        // GOLDEN: the imports emitted at the top of the island.
        assert_eq!(
            result.imports,
            vec![
                "import * as apiExports__ext from '@scope/api';".to_string(),
                "const apiExports = apiExports__ext.default ?? apiExports__ext;".to_string(),
            ]
        );
        // GOLDEN: the surviving island body (whitespace-normalized) — only the
        // consumer and the barrel-init shim remain; all nine unit declarations
        // are gone.
        let body: Vec<&str> = result
            .source
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(
            body,
            vec![
                "var theApi = apiInit();",
                "function apiInit() { return apiExports; }",
            ]
        );
    }

    #[test]
    fn moves_only_cluster_function_declarations() {
        let island = "function f1() { return 1; }\n\
                      globalThis.__init = setup();\n\
                      class C1 { m() { return f1(); } }\n\
                      var keep = f1();\n\
                      function other() { return 2; }\n";
        let extraction =
            extract_hoistable_cluster(island, &bindings(&["f1", "C1"]), &BTreeSet::new())
                .expect("hoistable cluster should extract");

        // Only the function declaration moves; the class stays put.
        assert_eq!(extraction.moved_bindings, bindings(&["f1"]));
        assert!(extraction.cluster_source.contains("function f1()"));
        assert!(!extraction.cluster_source.contains("class C1"));
        // The class, side-effecting init, the order-dependent var, and the
        // out-of-cluster function all stay put.
        assert!(extraction.remaining_source.contains("class C1"));
        assert!(
            extraction
                .remaining_source
                .contains("globalThis.__init = setup();")
        );
        assert!(extraction.remaining_source.contains("var keep = f1();"));
        assert!(extraction.remaining_source.contains("function other()"));
        assert!(!extraction.remaining_source.contains("function f1()"));
    }

    #[test]
    fn never_moves_a_class_even_when_clustered() {
        // A class whose `extends` base is an eager island binding must not move:
        // esbuild evaluates an imported cluster before the entry, so a relocated
        // `class B extends na {}` would run `extends na` before the entry defines
        // `na` (`var na = require("node:events")`) — a `Class extends undefined`
        // crash. Only the function in the cluster is eval-order-safe to lift.
        let island = "var na = require(\"node:events\");\n\
                      class B extends na { run() { return helper(); } }\n\
                      function helper() { return na; }\n";
        let extraction =
            extract_hoistable_cluster(island, &bindings(&["B", "helper"]), &BTreeSet::new())
                .expect("hoistable cluster should extract");
        assert_eq!(extraction.moved_bindings, bindings(&["helper"]));
        assert!(extraction.cluster_source.contains("function helper()"));
        assert!(!extraction.cluster_source.contains("class B"));
        // The class and its eager `extends` base stay together in the island.
        assert!(extraction.remaining_source.contains("var na = require"));
        assert!(extraction.remaining_source.contains("class B extends na"));
    }

    #[test]
    fn never_moves_a_reassigned_binding() {
        // `f1` is a function but is reassigned later, so it cannot become a
        // read-only import — it must stay in the island.
        let island = "function f1() { return 1; }\nf1 = wrap(f1);\n";
        let result = extract_hoistable_cluster(island, &bindings(&["f1"]), &bindings(&["f1"]));
        assert!(result.is_none(), "reassigned binding must not be extracted");
    }

    #[test]
    fn leaves_side_effect_only_island_untouched() {
        // No hoistable declarations to move -> None, so the caller emits the
        // island unchanged.
        let island = "globalThis.__init = setup();\nvar x = compute();\n";
        assert!(extract_hoistable_cluster(island, &bindings(&["x"]), &BTreeSet::new()).is_none());
    }

    #[test]
    fn assembles_a_cluster_file_with_imports_body_and_reexport() {
        let mut imports = BTreeMap::new();
        imports.insert("../runtime/shared.js".to_string(), bindings(&["helper"]));
        let file = assemble_cluster_file(
            "function f1() { return helper(); }",
            &bindings(&["f1"]),
            &imports,
        );
        assert!(file.contains("import { helper } from '../runtime/shared.js';"));
        assert!(file.contains("function f1()"));
        assert!(file.contains("export { f1 };"));
        // Import precedes the declaration which precedes the export.
        let import_at = file.find("import { helper }").expect("import line present");
        let decl_at = file.find("function f1").expect("declaration present");
        let export_at = file.find("export { f1 }").expect("export line present");
        assert!(import_at < decl_at && decl_at < export_at);
    }

    fn binding_to_cluster(pairs: &[(&str, usize)]) -> BTreeMap<BindingName, usize> {
        pairs
            .iter()
            .map(|(name, id)| (BindingName::new(*name), *id))
            .collect()
    }

    #[test]
    fn partitions_all_clusters_in_one_pass() {
        let island = "function a1() { return a2(); }\n\
                      function a2() { return 1; }\n\
                      globalThis.__init = run();\n\
                      function b1() { return 2; }\n\
                      var keep = a1();\n";
        let partition = partition_island_into_clusters(
            island,
            &binding_to_cluster(&[("a1", 0), ("a2", 0), ("b1", 1)]),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .expect("island should partition into clusters");

        assert_eq!(partition.clusters.len(), 2);
        let c0 = partition
            .clusters
            .iter()
            .find(|g| g.cluster_id == 0)
            .expect("cluster 0 present");
        assert_eq!(c0.moved_bindings, bindings(&["a1", "a2"]));
        assert!(c0.cluster_source.contains("function a1()"));
        assert!(c0.cluster_source.contains("function a2()"));
        let c1 = partition
            .clusters
            .iter()
            .find(|g| g.cluster_id == 1)
            .expect("cluster 1 present");
        assert_eq!(c1.moved_bindings, bindings(&["b1"]));
        // Side effect and order-dependent var stay; functions are gone.
        assert!(
            partition
                .remaining_source
                .contains("globalThis.__init = run();")
        );
        assert!(partition.remaining_source.contains("var keep = a1();"));
        assert!(!partition.remaining_source.contains("function a1()"));
        assert!(!partition.remaining_source.contains("function b1()"));
    }

    #[test]
    fn entry_imports_the_moved_bindings_back() {
        let statement = entry_import_for_cluster(&bindings(&["f1", "C1"]), "./island/cluster-0.js");
        assert_eq!(statement, "import { C1, f1 } from './island/cluster-0.js';");
    }

    #[test]
    fn moves_forced_exports_and_guard_vars_with_their_init() {
        // A recognized CJS module triple: `var EXPORTS = {}; var GUARD; function
        // INIT() {…}`. With the three bindings mapped to one cluster and the
        // exports/guard vars marked force-move, all three relocate together while
        // an unrelated eager `var` and side effect stay in the entry.
        let island = "var CH = {};\n\
                      var pAe;\n\
                      function EOt() { return pAe || (pAe = 1, CH._x = 1), CH; }\n\
                      var keep = 1;\n\
                      sideEffect();\n";
        let mut force = BTreeSet::new();
        force.insert(BindingName::new("CH"));
        force.insert(BindingName::new("pAe"));
        let partition = partition_island_into_clusters(
            island,
            &binding_to_cluster(&[("EOt", 7), ("CH", 7), ("pAe", 7)]),
            &BTreeSet::new(),
            &force,
            &BTreeSet::new(),
        )
        .expect("partition succeeds");

        let group = partition
            .clusters
            .iter()
            .find(|g| g.cluster_id == 7)
            .expect("cluster 7 exists");
        assert_eq!(group.moved_bindings, bindings(&["CH", "EOt", "pAe"]));
        assert!(group.cluster_source.contains("var CH = {};"));
        assert!(group.cluster_source.contains("var pAe;"));
        assert!(group.cluster_source.contains("function EOt()"));
        // The module triple is gone from the entry; unrelated statements remain.
        assert!(!partition.remaining_source.contains("function EOt()"));
        assert!(!partition.remaining_source.contains("var CH = {};"));
        assert!(!partition.remaining_source.contains("var pAe;"));
        assert!(partition.remaining_source.contains("var keep = 1;"));
        assert!(partition.remaining_source.contains("sideEffect();"));
    }

    #[test]
    fn does_not_move_a_non_forced_var() {
        // A plain `var` that is not a force-move variable stays in the entry even
        // when mapped to a cluster (only functions and forced vars relocate).
        let island = "var x = compute();\nfunction f() { return 1; }\n";
        let partition = partition_island_into_clusters(
            island,
            &binding_to_cluster(&[("x", 0), ("f", 0)]),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .expect("partition succeeds");
        assert!(partition.remaining_source.contains("var x = compute();"));
        let group = &partition.clusters[0];
        assert_eq!(group.moved_bindings, bindings(&["f"]));
    }

    #[test]
    fn moves_eval_order_safe_class_but_keeps_unsafe_one() {
        // `Safe` is listed in `movable_classes` and relocates; `Unsafe` is not
        // (its caller proved it touches an eager binding) and stays in the entry.
        let island = "class Safe extends Error {}\nclass Unsafe extends IslandBase {}\n";
        let mut movable = BTreeSet::new();
        movable.insert(BindingName::new("Safe"));
        let partition = partition_island_into_clusters(
            island,
            &binding_to_cluster(&[("Safe", 3), ("Unsafe", 3)]),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &movable,
        )
        .expect("partition succeeds");

        let group = &partition.clusters[0];
        assert_eq!(group.moved_bindings, bindings(&["Safe"]));
        assert!(group.cluster_source.contains("class Safe extends Error"));
        assert!(
            !partition
                .remaining_source
                .contains("class Safe extends Error")
        );
        assert!(
            partition
                .remaining_source
                .contains("class Unsafe extends IslandBase")
        );
    }

    #[test]
    fn splits_oversized_cluster_into_size_bounded_subclusters() {
        // Three 3-line functions with a 3-line budget: greedy packing yields one
        // function per sub-cluster, fresh ids after the first, bindings preserved.
        let group = ClusterGroup {
            cluster_id: 0,
            moved_bindings: bindings(&["a", "b", "c"]),
            cluster_source:
                "function a() {\n\treturn 1;\n}\nfunction b() {\n\treturn 2;\n}\nfunction c() {\n\treturn 3;\n}"
                    .to_string(),
        };
        let mut next_id = 100;
        let out = split_oversized_clusters(vec![group], 3, &mut next_id);
        assert_eq!(out.len(), 3, "{out:?}");
        assert!(out.iter().all(|g| g.cluster_source.lines().count() <= 3));
        assert_eq!(
            out[0].cluster_id, 0,
            "first sub-cluster keeps the original id"
        );
        assert_eq!(out[1].cluster_id, 100);
        assert_eq!(out[2].cluster_id, 101);
        let all: BTreeSet<BindingName> = out
            .iter()
            .flat_map(|g| g.moved_bindings.iter().cloned())
            .collect();
        assert_eq!(all, bindings(&["a", "b", "c"]), "every binding preserved");
    }

    #[test]
    fn splits_oversized_cluster_along_semantic_communities() {
        // Two reference-disjoint pairs: {a1↔a2} and {b1↔b2}. Community detection
        // recovers the two cohesive groups, so the oversized cluster splits along
        // them rather than by arbitrary source order.
        let island = "function a1() { return a2(); }\n\
                      function b1() { return b2(); }\n\
                      function a2() { return a1; }\n\
                      function b2() { return b1; }\n";
        let group = ClusterGroup {
            cluster_id: 0,
            moved_bindings: bindings(&["a1", "a2", "b1", "b2"]),
            cluster_source: island.to_string(),
        };
        let mut next_id = 100;
        // Budget (target = 8*4/5 = 6) holds one pair (6 tokens) but not both (12).
        let out = split_oversized_clusters(vec![group], 8, &mut next_id);
        assert_eq!(out.len(), 2, "splits into the two communities: {out:?}");
        // Each output file holds a cohesive pair, never a mix across communities.
        for group in &out {
            let has_a = group.moved_bindings.contains(&BindingName::new("a1"));
            let has_b = group.moved_bindings.contains(&BindingName::new("b1"));
            assert!(has_a != has_b, "community kept intact: {group:?}");
            if has_a {
                assert!(group.moved_bindings.contains(&BindingName::new("a2")));
            } else {
                assert!(group.moved_bindings.contains(&BindingName::new("b2")));
            }
        }
    }

    #[test]
    fn keeps_within_budget_cluster_unsplit() {
        let group = ClusterGroup {
            cluster_id: 5,
            moved_bindings: bindings(&["a"]),
            cluster_source: "function a() { return 1; }".to_string(),
        };
        let mut next_id = 100;
        let out = split_oversized_clusters(vec![group], 5000, &mut next_id);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].cluster_id, 5);
        assert_eq!(next_id, 100, "no new ids consumed for an in-budget cluster");
    }

    #[test]
    fn does_not_move_a_function_partly_outside_the_cluster() {
        // A statement binding two names, only one of which is in the cluster, is
        // not safe to split — keep it whole in the island.
        let island = "function a() { return 1; }\n";
        // `a` is in the cluster, so it moves; but verify the all-bindings rule by
        // excluding it from the cluster.
        assert!(extract_hoistable_cluster(island, &bindings(&["b"]), &BTreeSet::new()).is_none());
    }
}
