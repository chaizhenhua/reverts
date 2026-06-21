//! Community detection for splitting the eager entry island into module-sized
//! clusters.
//!
//! A scope-hoisting bundler flattens many source modules into one top-level
//! scope and minification erases every boundary marker, so the island is a
//! single connected blob: plain connected-components yields one component, and
//! label propagation collapses distinct modules across the shared-helper bridges
//! that connect them. What survives the transform is the *reference structure* —
//! an original module's bindings reference each other densely and reference
//! other modules' bindings sparsely. Modularity optimization recovers exactly
//! that: this is one level of the Louvain local-moving heuristic over the
//! undirected binding reference graph.
//!
//! The algorithm here is deterministic (fixed node order, smaller-community
//! tie-break) and operates on opaque node indices; callers map their binding
//! names to `0..n` and back.

use std::collections::{BTreeMap, BTreeSet};

use reverts_graph::{RuntimePrelude, RuntimePreludeBindingKind};
use reverts_ir::BindingName;

use crate::runtime_source_scan::value_identifiers_in_source;

/// Cluster a runtime prelude's eager (`SourceBacked`) island bindings into
/// modules by community detection over their reference graph.
///
/// Each eager binding is a node; an edge joins two eager bindings when one
/// reads the other. Reference sets come from the precomputed per-statement
/// `reads` on each snippet's sub-snippets (no re-parsing); a binding whose
/// snippet was never split into statements falls back to scanning its source.
/// Returns `binding -> module-cluster id`.
#[must_use]
pub(crate) fn cluster_island_prelude(prelude: &RuntimePrelude) -> BTreeMap<BindingName, usize> {
    let island_bindings: BTreeSet<BindingName> = prelude
        .bindings
        .iter()
        .filter(|(_, kind)| matches!(kind, RuntimePreludeBindingKind::SourceBacked))
        .map(|(binding, _)| binding.clone())
        .collect();

    let mut references: BTreeMap<BindingName, BTreeSet<BindingName>> = BTreeMap::new();
    for binding in &island_bindings {
        let mut refs = BTreeSet::new();
        if let Some(snippet) = prelude.snippets.get(binding) {
            if snippet.sub_snippets.is_empty() {
                for identifier in value_identifiers_in_source(snippet.source.as_str()) {
                    let read = BindingName::new(identifier);
                    if island_bindings.contains(&read) {
                        refs.insert(read);
                    }
                }
            } else {
                for sub in &snippet.sub_snippets {
                    for read in &sub.reads {
                        if island_bindings.contains(read) {
                            refs.insert(read.clone());
                        }
                    }
                }
            }
        }
        refs.remove(binding);
        references.insert(binding.clone(), refs);
    }

    cluster_bindings_by_references(&references)
}

/// Cluster island bindings into modules by community detection over their
/// reference graph.
///
/// `references[b]` is the set of bindings `b` reads or calls. Only edges between
/// two island bindings (both present as keys) form the graph — a reference to a
/// module-owned or imported binding is not an intra-island edge. The directed
/// reference relation is treated as an undirected simple graph for community
/// detection. Returns `binding -> cluster id` (contiguous from 0); an island
/// binding that references nothing and is referenced by nothing forms its own
/// singleton cluster.
pub(crate) fn cluster_bindings_by_references(
    references: &BTreeMap<BindingName, BTreeSet<BindingName>>,
) -> BTreeMap<BindingName, usize> {
    // Stable index for every island binding (the keys of `references`).
    let nodes: Vec<&BindingName> = references.keys().collect();
    let index: BTreeMap<&BindingName, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, &binding)| (binding, i))
        .collect();

    // Build a simple undirected graph: dedup neighbors so a mutual reference is
    // a single edge, and drop edges to non-island bindings.
    let mut neighbors: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); nodes.len()];
    for (binding, referenced) in references {
        let from = index[binding];
        for target in referenced {
            if let Some(&to) = index.get(target)
                && to != from
            {
                neighbors[from].insert(to);
                neighbors[to].insert(from);
            }
        }
    }
    let adjacency: Vec<Vec<usize>> = neighbors
        .into_iter()
        .map(|set| set.into_iter().collect())
        .collect();

    let communities = louvain_communities(nodes.len(), &adjacency);
    nodes
        .iter()
        .enumerate()
        .map(|(i, &binding)| (binding.clone(), communities[i]))
        .collect()
}

/// Partition `n` nodes into communities by one level of Louvain modularity
/// optimization over the undirected graph `adjacency`.
///
/// `adjacency[i]` lists the neighbors of node `i`; every undirected edge must
/// appear in both endpoints' lists. Self-loops are ignored. Returns a
/// `community[i]` label per node, relabeled to a contiguous `0..k` range in
/// order of first appearance. With no edges every node is its own community.
#[must_use]
pub(crate) fn louvain_communities(n: usize, adjacency: &[Vec<usize>]) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    let degree: Vec<f64> = adjacency.iter().map(|neighbors| neighbors.len() as f64).collect();
    let two_m: f64 = degree.iter().sum();
    if two_m == 0.0 {
        return (0..n).collect();
    }

    // Each node starts in its own community; `sigma_tot[c]` is the summed degree
    // of the nodes currently in community `c`.
    let mut community: Vec<usize> = (0..n).collect();
    let mut sigma_tot: Vec<f64> = degree.clone();

    let mut improved = true;
    let mut iterations = 0;
    while improved && iterations < MAX_ITERATIONS {
        improved = false;
        iterations += 1;
        for node in 0..n {
            let ki = degree[node];
            let current = community[node];
            // Tentatively isolate `node` from its community before scoring moves.
            sigma_tot[current] -= ki;

            // Sum edge weight from `node` into each neighboring community.
            let mut weight_to: Vec<(usize, f64)> = Vec::new();
            for &neighbor in &adjacency[node] {
                if neighbor == node {
                    continue;
                }
                let c = community[neighbor];
                match weight_to.iter_mut().find(|(community, _)| *community == c) {
                    Some((_, weight)) => *weight += 1.0,
                    None => weight_to.push((c, 1.0)),
                }
            }

            // Modularity gain of placing `node` into community `c` is
            // `ki_in(c) - ki * sigma_tot[c] / 2m`. Staying isolated scores 0, so
            // the current community must beat that to retain `node`.
            let mut best_community = current;
            let mut best_gain = weight_to
                .iter()
                .find(|(c, _)| *c == current)
                .map_or(0.0, |(_, ki_in)| ki_in - ki * sigma_tot[current] / two_m);
            for &(c, ki_in) in &weight_to {
                let gain = ki_in - ki * sigma_tot[c] / two_m;
                // Strictly better wins; an exact tie prefers the smaller id for
                // determinism, but never displaces the current community on a tie.
                if gain > best_gain + EPSILON
                    || (gain > best_gain - EPSILON && c < best_community)
                {
                    best_gain = gain;
                    best_community = c;
                }
            }

            sigma_tot[best_community] += ki;
            if best_community != current {
                community[node] = best_community;
                improved = true;
            }
        }
    }

    relabel_contiguous(&community)
}

const MAX_ITERATIONS: usize = 100;
const EPSILON: f64 = 1e-12;

/// Relabel arbitrary community ids to a contiguous `0..k` range in order of
/// first appearance, so the output is stable and dense.
fn relabel_contiguous(community: &[usize]) -> Vec<usize> {
    let mut remap: Vec<Option<usize>> = vec![None; community.len()];
    let mut next = 0;
    community
        .iter()
        .map(|&c| match remap[c] {
            Some(id) => id,
            None => {
                let id = next;
                remap[c] = Some(id);
                next += 1;
                id
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build symmetric adjacency lists from an undirected edge list.
    fn adjacency(n: usize, edges: &[(usize, usize)]) -> Vec<Vec<usize>> {
        let mut adjacency = vec![Vec::new(); n];
        for &(a, b) in edges {
            adjacency[a].push(b);
            adjacency[b].push(a);
        }
        adjacency
    }

    fn community_count(communities: &[usize]) -> usize {
        communities.iter().copied().max().map_or(0, |max| max + 1)
    }

    #[test]
    fn two_dense_cliques_joined_by_one_bridge_stay_separate() {
        // Two triangles {0,1,2} and {3,4,5} joined only by edge 0-3 — the shape a
        // shared helper makes between two original modules. Modularity keeps them
        // apart where label propagation would collapse them.
        let edges = [
            (0, 1),
            (0, 2),
            (1, 2),
            (3, 4),
            (3, 5),
            (4, 5),
            (0, 3),
        ];
        let communities = louvain_communities(6, &adjacency(6, &edges));
        assert_eq!(community_count(&communities), 2, "{communities:?}");
        assert_eq!(communities[0], communities[1]);
        assert_eq!(communities[1], communities[2]);
        assert_eq!(communities[3], communities[4]);
        assert_eq!(communities[4], communities[5]);
        assert_ne!(communities[0], communities[3]);
    }

    #[test]
    fn a_single_clique_is_one_community() {
        let edges = [(0, 1), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3)];
        let communities = louvain_communities(4, &adjacency(4, &edges));
        assert_eq!(community_count(&communities), 1, "{communities:?}");
    }

    #[test]
    fn disconnected_components_are_separate_communities() {
        let edges = [(0, 1), (2, 3)];
        let communities = louvain_communities(4, &adjacency(4, &edges));
        assert_eq!(community_count(&communities), 2);
        assert_eq!(communities[0], communities[1]);
        assert_eq!(communities[2], communities[3]);
        assert_ne!(communities[0], communities[2]);
    }

    #[test]
    fn isolated_nodes_with_no_edges_each_form_their_own_community() {
        let communities = louvain_communities(3, &adjacency(3, &[]));
        assert_eq!(community_count(&communities), 3);
    }

    #[test]
    fn three_cliques_in_a_chain_recover_three_communities() {
        // {0,1,2} - {3,4,5} - {6,7,8}, each pair joined by a single bridge.
        let edges = [
            (0, 1),
            (0, 2),
            (1, 2),
            (3, 4),
            (3, 5),
            (4, 5),
            (6, 7),
            (6, 8),
            (7, 8),
            (2, 3),
            (5, 6),
        ];
        let communities = louvain_communities(9, &adjacency(9, &edges));
        assert_eq!(community_count(&communities), 3, "{communities:?}");
    }

    #[test]
    fn empty_graph_is_empty() {
        assert!(louvain_communities(0, &[]).is_empty());
    }

    fn refs(pairs: &[(&str, &[&str])]) -> BTreeMap<BindingName, BTreeSet<BindingName>> {
        pairs
            .iter()
            .map(|(binding, referenced)| {
                (
                    BindingName::new(*binding),
                    referenced.iter().map(|r| BindingName::new(*r)).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn binding_reference_cliques_cluster_into_modules() {
        // Two modules' worth of bindings: {a1,a2,a3} reference each other,
        // {b1,b2,b3} reference each other, and a single cross-edge a1->b1 stands
        // in for a shared-helper call between modules.
        let references = refs(&[
            ("a1", &["a2", "a3", "b1"]),
            ("a2", &["a1", "a3"]),
            ("a3", &["a1", "a2"]),
            ("b1", &["b2", "b3"]),
            ("b2", &["b1", "b3"]),
            ("b3", &["b1", "b2"]),
        ]);
        let clusters = cluster_bindings_by_references(&references);

        let a = clusters[&BindingName::new("a1")];
        assert_eq!(a, clusters[&BindingName::new("a2")]);
        assert_eq!(a, clusters[&BindingName::new("a3")]);
        let b = clusters[&BindingName::new("b1")];
        assert_eq!(b, clusters[&BindingName::new("b2")]);
        assert_eq!(b, clusters[&BindingName::new("b3")]);
        assert_ne!(a, b, "the two reference cliques must be distinct modules");
    }

    fn source_backed_prelude(
        cliques: &[&[&str]],
        bridges: &[(&str, &str)],
    ) -> RuntimePrelude {
        use reverts_graph::{RuntimePreludeSnippet, RuntimePreludeSubSnippet};

        let mut bindings = BTreeMap::new();
        let mut snippets = BTreeMap::new();
        for clique in cliques {
            for &name in *clique {
                bindings.insert(
                    BindingName::new(name),
                    RuntimePreludeBindingKind::SourceBacked,
                );
                // Reads = the rest of the clique (a dense intra-module reference
                // set) plus any bridge targets from this node.
                let mut reads: BTreeSet<BindingName> = clique
                    .iter()
                    .filter(|&&other| other != name)
                    .map(|&other| BindingName::new(other))
                    .collect();
                for &(from, to) in bridges {
                    if from == name {
                        reads.insert(BindingName::new(to));
                    }
                }
                snippets.insert(
                    BindingName::new(name),
                    RuntimePreludeSnippet {
                        source: format!("var {name} = 1;"),
                        byte_start: 0,
                        sub_snippets: vec![RuntimePreludeSubSnippet {
                            source: format!("var {name} = 1;"),
                            byte_start: 0,
                            byte_end: 0,
                            defines: BTreeSet::from([BindingName::new(name)]),
                            reads,
                            writes: BTreeSet::new(),
                        }],
                        augmentations: Vec::new(),
                    },
                );
            }
        }
        RuntimePrelude {
            source_file_id: 1,
            source_file_path: "bundle.js".to_string(),
            source: String::new(),
            bindings,
            snippets,
            namespace_exports: Vec::new(),
            entrypoint: None,
        }
    }

    #[test]
    fn island_prelude_partitions_eager_bindings_into_reference_modules() {
        let prelude = source_backed_prelude(
            &[&["a1", "a2", "a3"], &["b1", "b2", "b3"]],
            &[("a1", "b1")],
        );
        let clusters = cluster_island_prelude(&prelude);
        assert_eq!(clusters.len(), 6);
        let a = clusters[&BindingName::new("a1")];
        assert_eq!(a, clusters[&BindingName::new("a2")]);
        assert_eq!(a, clusters[&BindingName::new("a3")]);
        let b = clusters[&BindingName::new("b1")];
        assert_eq!(b, clusters[&BindingName::new("b2")]);
        assert_eq!(b, clusters[&BindingName::new("b3")]);
        assert_ne!(a, b);
    }

    #[test]
    fn references_to_non_island_bindings_are_ignored() {
        // `x` references `imported` which is not an island binding (not a key);
        // that edge must not create a node or affect clustering.
        let references = refs(&[("x", &["imported", "y"]), ("y", &["x"])]);
        let clusters = cluster_bindings_by_references(&references);
        assert_eq!(clusters.len(), 2, "only island bindings are nodes");
        assert_eq!(clusters[&BindingName::new("x")], clusters[&BindingName::new("y")]);
    }
}
