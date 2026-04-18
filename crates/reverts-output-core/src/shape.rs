use reverts_ir::{BindingShapeSolution, DefUseGraph};

#[must_use]
pub fn solve_binding_shapes(graph: &DefUseGraph) -> BindingShapeSolution {
    let mut solution = BindingShapeSolution::default();
    for constraint in graph.constraints() {
        solution.add_constraint(constraint);
    }
    solution
}

#[cfg(test)]
mod tests {
    use reverts_ir::{
        BindingConstraint, BindingConstraintKind, BindingShape, DefUseGraph, ModuleId,
    };

    use super::solve_binding_shapes;

    #[test]
    fn callsite_forces_unresolved_binding_to_callable_shape() {
        let mut graph = DefUseGraph::default();
        graph.constrain(BindingConstraint::new(
            ModuleId(1),
            "zz",
            BindingConstraintKind::Call,
        ));

        let solution = solve_binding_shapes(&graph);

        assert_eq!(solution.shape_of(ModuleId(1), "zz"), BindingShape::Callable);
    }

    #[test]
    fn enum_iife_initializer_materializes_enum_object_shape() {
        let mut graph = DefUseGraph::default();
        graph.constrain(BindingConstraint::new(
            ModuleId(1),
            "NativeModuleType",
            BindingConstraintKind::EnumInitializer,
        ));

        let solution = solve_binding_shapes(&graph);

        assert_eq!(
            solution.shape_of(ModuleId(1), "NativeModuleType"),
            BindingShape::EnumObject
        );
    }
}
