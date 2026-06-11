#![allow(dead_code)]

// CFG nodes don't carry byte spans in v1; we hash the entire module CFG.
// The span parameter is reserved for when per-function CFG slicing is added.

use reverts_ir::hash::{FNV_OFFSET_BASIS, update_fnv1a};
use reverts_ir::{ByteRange, ControlFlowEdgeKind, ControlFlowGraph, ControlFlowNodeKind, ModuleId};

#[must_use]
pub fn compute(cfg: &ControlFlowGraph, module_id: ModuleId, _span: ByteRange) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, b"cfg|");

    // Hash all nodes for this module in ascending id order.
    let mut nodes: Vec<_> = cfg.nodes_for(module_id).iter().collect();
    nodes.sort_by_key(|n| n.id.0);
    for node in &nodes {
        update_fnv1a(&mut hash, b"n:");
        update_fnv1a(&mut hash, node_kind_tag(node.kind).as_bytes());
        update_fnv1a(&mut hash, b"|");
    }

    // Hash all edges for this module in ascending (from, to) order.
    let mut edges: Vec<_> = cfg.edges_for(module_id).iter().collect();
    edges.sort_by_key(|e| (e.from.0, e.to.0));
    for edge in &edges {
        update_fnv1a(&mut hash, b"e:");
        update_fnv1a(&mut hash, edge_kind_tag(edge.kind).as_bytes());
        update_fnv1a(&mut hash, b"|");
    }

    hash
}

const fn node_kind_tag(k: ControlFlowNodeKind) -> &'static str {
    match k {
        ControlFlowNodeKind::Entry => "entry",
        ControlFlowNodeKind::Exit => "exit",
        ControlFlowNodeKind::Statement => "stmt",
        ControlFlowNodeKind::Branch => "branch",
        ControlFlowNodeKind::Loop => "loop",
        ControlFlowNodeKind::Return => "return",
        ControlFlowNodeKind::Throw => "throw",
    }
}

const fn edge_kind_tag(k: ControlFlowEdgeKind) -> &'static str {
    match k {
        ControlFlowEdgeKind::Entry => "entry",
        ControlFlowEdgeKind::Sequential => "seq",
        ControlFlowEdgeKind::Conditional => "cond",
        ControlFlowEdgeKind::LoopBack => "back",
        ControlFlowEdgeKind::Termination => "term",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reverts_ir::{ByteRange, ControlFlowGraph, ModuleId};

    #[test]
    fn cfg_hash_is_deterministic_on_empty_cfg() {
        let cfg = ControlFlowGraph::default();
        let h1 = compute(&cfg, ModuleId(1), ByteRange::new(0, 100));
        let h2 = compute(&cfg, ModuleId(1), ByteRange::new(0, 100));
        assert_eq!(h1, h2);
    }
}
