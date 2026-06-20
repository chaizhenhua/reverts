use std::collections::BTreeSet;

use reverts_ir::ModuleId;

use super::PackageSource;

#[derive(Debug, Clone, PartialEq, Eq)]
/// Structural fingerprint used to compare one bundle module with package source candidates.
pub struct ModuleMatchFingerprint {
    /// module id in the input bundle.
    pub module_id: ModuleId,
    /// optional package hint attached to the module.
    pub package_name: Option<String>,
    /// optional concrete package version hint attached to the module.
    pub package_version: Option<String>,
    /// stable hash of the AST-normalized source body.
    pub normalized_source_hash: String,
    /// Stable hashes of normalized source variants produced by one
    /// normalization pass. Includes `normalized_source_hash`.
    pub normalized_source_hashes: BTreeSet<String>,
    /// AST-derived function signature hashes.
    pub function_signature_hashes: BTreeSet<String>,
    /// Order-insensitive hashes of normalized top-level declaration statements.
    pub top_level_declaration_hashes: BTreeSet<String>,
    /// Order-insensitive import/export surface hashes.
    pub import_export_surface_hashes: BTreeSet<String>,
    /// Class/object/prototype member multiset hashes.
    pub class_member_hashes: BTreeSet<String>,
    /// Local statement-window hashes.
    pub statement_window_hashes: BTreeSet<String>,
    /// Block and branch shape hashes.
    pub block_branch_hashes: BTreeSet<String>,
    /// string literal anchors collected from the AST.
    pub string_anchors: BTreeSet<String>,
    /// Multi-axis function structure anchors shared by package and cross-version matching.
    pub function_axis_anchors: BTreeSet<String>,
    /// JSX/React shape anchors extracted from raw TSX or lowered JSX-runtime calls.
    pub jsx_react_shape_anchors: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Fingerprint for one cached package source file.
pub struct PackageSourceFingerprint<'a> {
    /// cached source candidate.
    pub source: &'a PackageSource,
    /// stable hash of the AST-normalized source body.
    pub normalized_source_hash: String,
    /// Stable hashes of normalized source variants produced by one
    /// normalization pass. Includes `normalized_source_hash`.
    pub normalized_source_hashes: BTreeSet<String>,
    /// AST-derived function signature hashes.
    pub function_signature_hashes: BTreeSet<String>,
    /// Order-insensitive hashes of normalized top-level declaration statements.
    pub top_level_declaration_hashes: BTreeSet<String>,
    /// Order-insensitive import/export surface hashes.
    pub import_export_surface_hashes: BTreeSet<String>,
    /// Class/object/prototype member multiset hashes.
    pub class_member_hashes: BTreeSet<String>,
    /// Local statement-window hashes.
    pub statement_window_hashes: BTreeSet<String>,
    /// Block and branch shape hashes.
    pub block_branch_hashes: BTreeSet<String>,
    /// string literal anchors collected from the AST.
    pub string_anchors: BTreeSet<String>,
    /// Multi-axis function structure anchors shared by package and cross-version matching.
    pub function_axis_anchors: BTreeSet<String>,
    /// JSX/React shape anchors extracted from raw TSX or lowered JSX-runtime calls.
    pub jsx_react_shape_anchors: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Version bucket for one package.
pub struct PackageVersionCandidate<'a> {
    /// npm package name.
    pub package_name: String,
    /// concrete package version.
    pub package_version: String,
    /// cached source files belonging to this package version.
    pub sources: Vec<PackageSourceFingerprint<'a>>,
}
