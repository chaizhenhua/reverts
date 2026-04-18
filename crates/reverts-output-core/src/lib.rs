pub mod audit;
pub mod emitter;
pub mod entry;
pub mod package;
pub mod shape;

pub use audit::audit_def_use_graph;
pub use emitter::{BindingMaterialization, materialize_binding};
pub use entry::render_cli_dispatcher;
pub use package::{ImportDecision, PackageImportResolver};
pub use shape::solve_binding_shapes;
