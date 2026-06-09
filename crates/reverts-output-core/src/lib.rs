pub mod audit;
pub mod emitter;
pub mod entry;
pub mod package;
pub mod parse_audit;
pub mod shape;

pub use audit::audit_def_use_graph;
pub use emitter::{BindingMaterialization, materialize_binding_from_source};
pub use entry::render_cli_dispatcher;
pub use package::{ImportDecision, PackageImportResolver};
pub use parse_audit::{EmittedFile, audit_emitted_files_parse};
pub use shape::solve_binding_shapes;
