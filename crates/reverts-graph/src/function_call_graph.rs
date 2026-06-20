//! Explicit, scope-resolved intra-module function call graph.
//!
//! A first-class peer of the module import graph (`ImportExportGraph`) and the
//! variable def-use graph (`DefUseGraph`). Nodes are a module's top-level
//! function bindings; a forward edge `caller → callee` means `caller`'s body
//! (including nested closures) calls the top-level function `callee` in the
//! SAME module.
//!
//! Built from [`crate::function_call_edges`], which resolves callee names within
//! each module's own top-level function universe — so minified, module-local
//! names (the reused `A`/`W`/`X`) never conflate across modules. Cross-module
//! call edges are intentionally out of scope: those route through the module
//! import graph plus export tables, which already carry the owner mapping.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use reverts_ir::{BindingName, ModuleId};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FunctionCallGraph {
    /// `module → caller → callees` forward adjacency. Mirrors `ImportExportGraph`,
    /// which also stores only the forward direction; reverse queries scan.
    calls: BTreeMap<ModuleId, BTreeMap<BindingName, BTreeSet<BindingName>>>,
}

impl FunctionCallGraph {
    pub(crate) fn record(&mut self, module_id: ModuleId, caller: BindingName, callee: BindingName) {
        self.calls
            .entry(module_id)
            .or_default()
            .entry(caller)
            .or_default()
            .insert(callee);
    }

    /// Direct callees of `caller` within `module_id`.
    #[must_use]
    pub fn callees_of(&self, module_id: ModuleId, caller: &BindingName) -> BTreeSet<BindingName> {
        self.calls
            .get(&module_id)
            .and_then(|module| module.get(caller))
            .cloned()
            .unwrap_or_default()
    }

    /// Direct callers of `callee` within `module_id` (reverse adjacency, scanned —
    /// like `ImportExportGraph` the graph keeps only forward edges).
    #[must_use]
    pub fn callers_of(&self, module_id: ModuleId, callee: &BindingName) -> BTreeSet<BindingName> {
        let Some(module) = self.calls.get(&module_id) else {
            return BTreeSet::new();
        };
        module
            .iter()
            .filter(|(_, callees)| callees.contains(callee))
            .map(|(caller, _)| caller.clone())
            .collect()
    }

    /// Top-level functions transitively reachable by calls from `roots` within
    /// `module_id` (forward BFS; `roots` are included in the result).
    #[must_use]
    pub fn reachable_from<'a>(
        &self,
        module_id: ModuleId,
        roots: impl IntoIterator<Item = &'a BindingName>,
    ) -> BTreeSet<BindingName> {
        let mut seen = BTreeSet::new();
        let mut queue: VecDeque<BindingName> = VecDeque::new();
        for root in roots {
            if seen.insert(root.clone()) {
                queue.push_back(root.clone());
            }
        }
        while let Some(node) = queue.pop_front() {
            if let Some(callees) = self.calls.get(&module_id).and_then(|m| m.get(&node)) {
                for callee in callees {
                    if seen.insert(callee.clone()) {
                        queue.push_back(callee.clone());
                    }
                }
            }
        }
        seen
    }

    /// The `caller → callees` adjacency for one module, if any edges exist.
    #[must_use]
    pub fn module_calls(
        &self,
        module_id: ModuleId,
    ) -> Option<&BTreeMap<BindingName, BTreeSet<BindingName>>> {
        self.calls.get(&module_id)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.calls.values().all(BTreeMap::is_empty)
    }

    /// Total number of forward caller→callee edges across all modules.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.calls
            .values()
            .flat_map(BTreeMap::values)
            .map(BTreeSet::len)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> BindingName {
        BindingName::new(s)
    }

    #[test]
    fn forward_reverse_and_transitive_queries() {
        let mut graph = FunctionCallGraph::default();
        // module 1: a → b → c, and a → c
        graph.record(ModuleId(1), name("a"), name("b"));
        graph.record(ModuleId(1), name("a"), name("c"));
        graph.record(ModuleId(1), name("b"), name("c"));
        // a different module's edges must not leak into module 1's queries.
        graph.record(ModuleId(2), name("a"), name("z"));

        assert_eq!(
            graph.callees_of(ModuleId(1), &name("a")),
            BTreeSet::from([name("b"), name("c")]),
        );
        assert_eq!(
            graph.callers_of(ModuleId(1), &name("c")),
            BTreeSet::from([name("a"), name("b")]),
        );
        // transitive reachability includes roots and is module-scoped.
        assert_eq!(
            graph.reachable_from(ModuleId(1), [&name("a")]),
            BTreeSet::from([name("a"), name("b"), name("c")]),
        );
        assert_eq!(
            graph.reachable_from(ModuleId(1), [&name("b")]),
            BTreeSet::from([name("b"), name("c")]),
        );
        assert!(
            graph
                .callees_of(ModuleId(1), &name("a"))
                .contains(&name("b"))
        );
        assert!(
            !graph
                .callees_of(ModuleId(2), &name("a"))
                .contains(&name("b"))
        );
        assert_eq!(graph.edge_count(), 4);
    }

    #[test]
    fn self_recursion_is_a_valid_edge() {
        let mut graph = FunctionCallGraph::default();
        graph.record(ModuleId(1), name("loop"), name("loop"));
        assert_eq!(
            graph.callees_of(ModuleId(1), &name("loop")),
            BTreeSet::from([name("loop")]),
        );
        assert_eq!(
            graph.reachable_from(ModuleId(1), [&name("loop")]),
            BTreeSet::from([name("loop")]),
        );
    }
}
