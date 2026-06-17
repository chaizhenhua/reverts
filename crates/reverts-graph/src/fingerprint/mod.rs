pub mod access;
pub mod ast;
pub mod binding_pattern;
pub mod callee_set;
pub mod cfg;
pub mod effect_pattern;
pub mod extractor;
pub mod literal_anchor;
pub mod literal_shape;
pub mod return_pattern;
pub mod structural_anchor;
pub mod throw_set;

pub use extractor::{ExtractedFunction, FunctionExtractor};

pub mod import_specifiers;
pub use import_specifiers::extract_import_specifiers;

pub mod property_names;
pub use property_names::extract_property_names;
