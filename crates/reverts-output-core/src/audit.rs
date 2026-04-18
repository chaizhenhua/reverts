use reverts_ir::DefUseGraph;
use reverts_observe::{AuditFinding, AuditReport, FindingCode};

#[must_use]
pub fn audit_def_use_graph(graph: &DefUseGraph) -> AuditReport {
    let mut report = AuditReport::default();
    for (module_id, binding) in graph.unresolved_reads() {
        report.push(
            AuditFinding::error(
                FindingCode::MissingDefinition,
                format!("binding '{binding}' is read without a local definition or import"),
            )
            .with_module(module_id.0.to_string())
            .with_binding(binding.as_str()),
        );
    }
    report
}

#[cfg(test)]
mod tests {
    use reverts_ir::{DefUseGraph, ModuleId};
    use reverts_observe::FindingCode;

    use super::audit_def_use_graph;

    #[test]
    fn read_without_definition_or_import_is_an_audit_finding() {
        let mut graph = DefUseGraph::default();
        graph.read(ModuleId(1), "NativeModuleType");

        let report = audit_def_use_graph(&graph);

        assert!(report.has(FindingCode::MissingDefinition));
    }

    #[test]
    fn imported_read_is_clean() {
        let mut graph = DefUseGraph::default();
        graph.import(ModuleId(1), "NativeModuleType");
        graph.read(ModuleId(1), "NativeModuleType");

        let report = audit_def_use_graph(&graph);

        assert!(report.is_clean());
    }
}
