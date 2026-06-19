use std::collections::{BTreeMap, BTreeSet};

/// Directed graph-neighborhood evidence for one candidate match.
///
/// `known_edges` counts neighbors on the left graph whose corresponding right
/// node is already known by seed matches. `matched_edges` counts how many of
/// those seeded neighbors are adjacent to the candidate on the right graph in
/// the same direction (outgoing or incoming).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GraphNeighborhoodEvidence {
    pub matched_edges: usize,
    pub known_edges: usize,
}

impl GraphNeighborhoodEvidence {
    #[must_use]
    pub fn coverage(self) -> f64 {
        if self.known_edges == 0 {
            0.0
        } else {
            self.matched_edges as f64 / self.known_edges as f64
        }
    }
}

/// Score candidate right-graph nodes by preserving the neighborhood of each
/// left-graph node under a set of seed matches.
///
/// This is intentionally package-agnostic: package matching can seed it with
/// exact/structural package-source proofs, while source matching can seed it
/// with anchor/structural source-file matches.
#[must_use]
pub fn graph_neighborhood_support<L, R>(
    left_outgoing: &BTreeMap<L, BTreeSet<L>>,
    left_incoming: &BTreeMap<L, BTreeSet<L>>,
    right_outgoing: &BTreeMap<R, BTreeSet<R>>,
    right_incoming: &BTreeMap<R, BTreeSet<R>>,
    seed_matches: &BTreeMap<L, R>,
) -> BTreeMap<L, BTreeMap<R, GraphNeighborhoodEvidence>>
where
    L: Ord + Clone,
    R: Ord + Clone,
{
    let mut support = BTreeMap::<L, BTreeMap<R, GraphNeighborhoodEvidence>>::new();
    for left_node in left_outgoing.keys().chain(left_incoming.keys()) {
        let outgoing_refs = left_outgoing
            .get(left_node)
            .into_iter()
            .flatten()
            .filter_map(|neighbor| seed_matches.get(neighbor))
            .collect::<BTreeSet<_>>();
        let incoming_refs = left_incoming
            .get(left_node)
            .into_iter()
            .flatten()
            .filter_map(|neighbor| seed_matches.get(neighbor))
            .collect::<BTreeSet<_>>();
        let known_edges = outgoing_refs.len() + incoming_refs.len();
        if known_edges == 0 {
            continue;
        }

        let mut candidate_scores = BTreeMap::<R, usize>::new();
        for outgoing_ref in outgoing_refs {
            for candidate in right_incoming.get(outgoing_ref).into_iter().flatten() {
                *candidate_scores.entry(candidate.clone()).or_default() += 1;
            }
        }
        for incoming_ref in incoming_refs {
            for candidate in right_outgoing.get(incoming_ref).into_iter().flatten() {
                *candidate_scores.entry(candidate.clone()).or_default() += 1;
            }
        }
        if !candidate_scores.is_empty() {
            support.insert(
                left_node.clone(),
                candidate_scores
                    .into_iter()
                    .map(|(right_node, matched_edges)| {
                        (
                            right_node,
                            GraphNeighborhoodEvidence {
                                matched_edges,
                                known_edges,
                            },
                        )
                    })
                    .collect(),
            );
        }
    }
    support
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_neighborhood_support_uses_outgoing_and_incoming_seeded_neighbors() {
        let left_outgoing = BTreeMap::from([
            ("subject", BTreeSet::from(["dep"])),
            ("parent", BTreeSet::from(["subject"])),
            ("dep", BTreeSet::new()),
        ]);
        let left_incoming = BTreeMap::from([
            ("subject", BTreeSet::from(["parent"])),
            ("parent", BTreeSet::new()),
            ("dep", BTreeSet::from(["subject"])),
        ]);
        let right_outgoing = BTreeMap::from([
            ("candidate", BTreeSet::from(["ref_dep"])),
            ("ref_parent", BTreeSet::from(["candidate"])),
            ("unrelated", BTreeSet::from(["ref_dep"])),
        ]);
        let right_incoming = BTreeMap::from([
            ("candidate", BTreeSet::from(["ref_parent"])),
            ("ref_dep", BTreeSet::from(["candidate", "unrelated"])),
        ]);
        let seed_matches = BTreeMap::from([("dep", "ref_dep"), ("parent", "ref_parent")]);

        let support = graph_neighborhood_support(
            &left_outgoing,
            &left_incoming,
            &right_outgoing,
            &right_incoming,
            &seed_matches,
        );
        let candidate = support["subject"]["candidate"];
        assert_eq!(
            candidate,
            GraphNeighborhoodEvidence {
                matched_edges: 2,
                known_edges: 2,
            }
        );
        assert_eq!(candidate.coverage(), 1.0);
        assert_eq!(support["subject"]["unrelated"].matched_edges, 1);
    }
}
