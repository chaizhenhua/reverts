mod cascade;
mod cascade_match;
mod prelude_anchor;
pub mod structural_bag;

pub use cascade::{GlobalAssignment, assign_globally, cascade_candidates, match_function};
pub use cascade_match::{CascadeMatchReport, CascadeOwnershipMatch, match_with_cascade};
pub use prelude_anchor::{PreludeBindingAnchor, PreludeBindingSource, anchor_prelude_bindings};
pub use structural_bag::{
    StructuralBag, StructuralBagMatchReport, build_structural_bag, match_structural_bags,
    match_structural_bags_with_excluded_modules, score_structural_bags,
};
